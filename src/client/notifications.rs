use std::io;

use tracing::{debug, warn};

use crate::protocol::NotifyKind;

use super::shell;

#[cfg(not(windows))]
use crate::platform::show_desktop_notification as show_untargeted_system_notification;

#[cfg(windows)]
fn queue_system_notification(
    task: impl FnOnce() -> io::Result<bool> + Send + 'static,
) -> io::Result<bool> {
    type Task = Box<dyn FnOnce() -> io::Result<bool> + Send>;
    static QUEUE: std::sync::OnceLock<tokio::sync::mpsc::UnboundedSender<Task>> =
        std::sync::OnceLock::new();
    let queue = QUEUE.get_or_init(|| {
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel::<Task>();
        // Preserve replacement order while keeping native waits off the client loop.
        tokio::spawn(async move {
            while let Some(task) = receiver.recv().await {
                let result = tokio::task::spawn_blocking(task)
                    .await
                    .unwrap_or_else(|err| Err(io::Error::other(err)));
                if let Err(err) = result {
                    warn!(err = %err, "failed to emit system notification");
                }
            }
        });
        sender
    });
    queue.send(Box::new(task)).map_err(|_| {
        io::Error::new(
            io::ErrorKind::BrokenPipe,
            "notification delivery queue closed",
        )
    })?;
    Ok(true)
}

#[cfg(not(windows))]
enum DesktopNotification {
    Plain {
        title: String,
        body: Option<String>,
    },
    Clickable {
        title: String,
        body: Option<String>,
        on_click: crate::platform::NotificationClick,
    },
}

/// Hands desktop notifications to one long-lived worker so their external notifier
/// commands (often hundreds of milliseconds each) never stall the client loop, run in
/// arrival order, and cannot pile up unbounded behind a slow notifier.
#[cfg(not(windows))]
fn queue_desktop_notification(notification: DesktopNotification) {
    const QUEUE_DEPTH: usize = 16;
    static QUEUE: std::sync::OnceLock<Option<std::sync::mpsc::SyncSender<DesktopNotification>>> =
        std::sync::OnceLock::new();
    let queue = QUEUE.get_or_init(|| {
        let (sender, receiver) = std::sync::mpsc::sync_channel::<DesktopNotification>(QUEUE_DEPTH);
        std::thread::Builder::new()
            .name("herdr-desktop-notify".into())
            .spawn(move || {
                for notification in receiver {
                    let result = match notification {
                        DesktopNotification::Plain { title, body } => {
                            crate::platform::show_desktop_notification(&title, body.as_deref())
                        }
                        DesktopNotification::Clickable {
                            title,
                            body,
                            on_click,
                        } => crate::platform::show_clickable_desktop_notification(
                            &title,
                            body.as_deref(),
                            on_click,
                        ),
                    };
                    if let Err(err) = result {
                        warn!(err = %err, "failed to emit system notification");
                    }
                }
            })
            .map_err(|err| warn!(err = %err, "failed to start desktop notification worker"))
            .ok()
            .map(|_| sender)
    });
    let Some(sender) = queue else {
        return;
    };
    if let Err(err) = sender.try_send(notification) {
        warn!(err = %err, "dropped system notification; notifier queue is full or closed");
    }
}

/// How a terminal-delivered notification reaches the user, best click path first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TerminalNotificationRoute {
    /// The terminal reports the click itself (kitty), locally or over SSH.
    ReportingTerminal,
    /// A local desktop notifier that reports the click.
    Desktop,
    /// A Rootshell push carrying the pane route.
    Rootshell,
    /// A terminal escape; herdr returns to the pane when the terminal regains focus.
    Terminal,
}

/// What the client knows about its terminal when it delivers a terminal toast.
#[derive(Clone, Copy, Debug, Default)]
struct TerminalNotificationContext {
    /// The notification names a pane the click can return to.
    has_target: bool,
    /// The terminal reports notification clicks back to herdr (kitty).
    terminal_reports_clicks: bool,
    /// The terminal shows OSC 9/99 notifications itself (Ghostty, iTerm2, WezTerm).
    terminal_shows_notifications: bool,
    /// The client runs over SSH or a remote endpoint.
    remote: bool,
    /// A local desktop notifier can report the click.
    desktop_reports_clicks: bool,
    /// The SSH session comes from Rootshell and rootshell-notify is configured.
    rootshell: bool,
}

fn terminal_notification_route(context: TerminalNotificationContext) -> TerminalNotificationRoute {
    if context.terminal_reports_clicks {
        TerminalNotificationRoute::ReportingTerminal
    } else if context.remote && context.rootshell {
        // Rootshell identifies as Ghostty (`TERM=xterm-ghostty`), but its escapes
        // are not seen while the app is in the background; only a push reaches it.
        TerminalNotificationRoute::Rootshell
    } else if context.terminal_shows_notifications {
        // The user already sees these toasts; clicking one refocuses the terminal,
        // which returns to the pane through the focus-return fallback.
        TerminalNotificationRoute::Terminal
    } else if !context.remote
        && context.desktop_reports_clicks
        && (context.has_target || cfg!(windows))
    {
        TerminalNotificationRoute::Desktop
    } else {
        TerminalNotificationRoute::Terminal
    }
}

/// The pane a Rootshell push may route to: only the local server's panes are
/// reachable through the socket rootshell-notify is given.
fn rootshell_route_pane(activation: &shell::ClientNotificationActivation) -> Option<String> {
    (activation.target.endpoint_id == super::endpoint::ClientEndpointId::Local)
        .then(|| activation.target.pane_id.clone())
}

/// Rootshell marks the sessions it opens with `LC_ROOTSHELL_PANE` or
/// `LC_TERMINAL=rootshell`, matching the SSH detection in `terminal_setup`.
fn rootshell_session() -> bool {
    (std::env::var_os("LC_ROOTSHELL_PANE").is_some()
        || std::env::var_os("LC_TERMINAL").is_some_and(|value| value == "rootshell"))
        && crate::rootshell_notify::find_rootshell_notify().is_some()
}

#[cfg(windows)]
fn show_untargeted_system_notification(title: &str, body: Option<&str>) -> io::Result<bool> {
    let title = title.to_owned();
    let body = body.map(str::to_owned);
    queue_system_notification(move || {
        crate::platform::show_desktop_notification(&title, body.as_deref())
    })
}

/// Delivers this tick's notification effects and returns the activation tokens whose
/// channel cannot report a click, so the shell can focus their pane when herdr regains
/// focus instead.
pub(super) fn handle_shell_notification_effects(
    effects: Vec<shell::ClientShellNotificationEffect>,
    sound_config: &crate::config::SoundConfig,
    event_tx: &tokio::sync::mpsc::Sender<super::events::ClientLoopEvent>,
) -> Vec<String> {
    let mut focus_activations = Vec::new();
    if effects.is_empty() {
        return focus_activations;
    }

    // In SSH/remote sessions, sound and desktop system notifications are not
    // emitted locally. They become a rootshell push instead, while terminal
    // toasts and the visual bell still render in the attached terminal.
    let is_ssh_or_remote = super::is_remote_or_ssh_session();
    let mut ssh_system_info: Option<(String, Option<String>, Option<String>)> = None;
    let mut ssh_sound_status: Option<crate::rootshell_notify::PushStatus> = None;

    for effect in effects {
        match effect {
            shell::ClientShellNotificationEffect::Sound { sound, agent } => {
                let agent = agent.as_deref().and_then(crate::detect::parse_agent_label);
                if sound_config.allows(agent) {
                    if is_ssh_or_remote {
                        ssh_sound_status = Some(sound.into());
                    } else {
                        crate::sound::play(sound, sound_config);
                    }
                }
            }
            shell::ClientShellNotificationEffect::VisualBell => {
                // The shell already inverts its composed frame for 60 ms; toggling the
                // outer terminal's reverse video (DECSCNM) as well could cancel that flash
                // or add a second one, so only ring the terminal bell here.
                if let Err(err) =
                    crate::terminal_effects::write_terminal_bells(&mut io::stdout(), 1)
                {
                    warn!(err = %err, "failed to emit terminal bell");
                }
            }
            shell::ClientShellNotificationEffect::Terminal {
                title,
                body,
                activation,
            } => {
                let route = terminal_notification_route(TerminalNotificationContext {
                    has_target: activation.is_some(),
                    terminal_reports_clicks: crate::terminal_notify::backend_reports_activation(),
                    terminal_shows_notifications: crate::terminal_notify::detect_backend()
                        .is_some(),
                    remote: is_ssh_or_remote,
                    desktop_reports_clicks:
                        crate::platform::clickable_desktop_notifications_available(),
                    rootshell: is_ssh_or_remote && rootshell_session(),
                });
                match route {
                    TerminalNotificationRoute::ReportingTerminal => {
                        if let Err(err) = crate::terminal_notify::show_notification_with_activation(
                            &title,
                            body.as_deref(),
                            activation
                                .as_ref()
                                .map(|activation| activation.token.as_str()),
                        ) {
                            warn!(err = %err, "failed to emit terminal notification");
                        }
                    }
                    TerminalNotificationRoute::Desktop => {
                        #[cfg(windows)]
                        if let Err(err) = show_system_notification(
                            &title,
                            body.as_deref(),
                            activation.map(|activation| activation.target),
                            event_tx,
                        ) {
                            warn!(err = %err, "failed to emit system notification");
                        }
                        #[cfg(not(windows))]
                        if let Some(activation) = activation {
                            let event_tx = event_tx.clone();
                            let clicked = activation.token;
                            queue_desktop_notification(DesktopNotification::Clickable {
                                title,
                                body,
                                on_click: std::sync::Arc::new(move || {
                                    let _ = event_tx.try_send(
                                        super::events::ClientLoopEvent::NotificationTokenActivated(
                                            clicked.clone(),
                                        ),
                                    );
                                }),
                            });
                        }
                    }
                    TerminalNotificationRoute::Rootshell => {
                        let pane = activation.as_ref().and_then(rootshell_route_pane);
                        if let Some(activation) = activation {
                            focus_activations.push(activation.token);
                        }
                        ssh_system_info = Some((title, body, pane));
                    }
                    TerminalNotificationRoute::Terminal => {
                        if let Some(activation) = activation {
                            focus_activations.push(activation.token);
                        }
                        if let Err(err) =
                            crate::terminal_notify::show_notification(&title, body.as_deref())
                        {
                            warn!(err = %err, "failed to emit terminal notification");
                        }
                    }
                }
            }
            shell::ClientShellNotificationEffect::System {
                title,
                body,
                #[cfg(windows)]
                target,
            } => {
                if is_ssh_or_remote {
                    ssh_system_info = Some((title, body, None));
                } else {
                    #[cfg(windows)]
                    if let Err(err) =
                        show_system_notification(&title, body.as_deref(), target, event_tx)
                    {
                        warn!(err = %err, "failed to emit system notification");
                    }
                    // Keep notifier commands off the client loop so frames (including
                    // the visual bell) and input are never stalled behind them.
                    #[cfg(not(windows))]
                    queue_desktop_notification(DesktopNotification::Plain { title, body });
                }
            }
        }
    }

    if is_ssh_or_remote {
        if let Some((title, body, pane)) = ssh_system_info {
            let status = ssh_sound_status.unwrap_or(crate::rootshell_notify::PushStatus::Info);
            crate::rootshell_notify::send_push_notification(
                &title,
                body.as_deref(),
                status,
                pane.as_deref(),
            );
        } else if let Some(status) = ssh_sound_status {
            crate::rootshell_notify::send_push_notification("Herdr", None, status, None);
        }

        if ssh_sound_status.is_some() {
            let _ = crate::terminal_effects::write_terminal_bells(&mut io::stdout(), 1);
        }
    }
    focus_activations
}

#[cfg(windows)]
fn show_system_notification(
    title: &str,
    body: Option<&str>,
    target: Option<shell::ClientSystemNotificationTarget>,
    event_tx: &tokio::sync::mpsc::Sender<super::events::ClientLoopEvent>,
) -> io::Result<bool> {
    let Some(target) = target else {
        return show_untargeted_system_notification(title, body);
    };
    let key = serde_json::to_string(&(
        &target.endpoint_id.storage_key(),
        &target.boot_id,
        &target.pane_id,
    ))
    .map_err(io::Error::other)?;
    let event_tx = event_tx.clone();
    let runtime = tokio::runtime::Handle::current();
    let title = title.to_owned();
    let body = body.map(str::to_owned);
    queue_system_notification(move || {
        crate::platform::show_actionable_desktop_notification(
            &title,
            body.as_deref(),
            key,
            std::sync::Arc::new(move || {
                let event_tx = event_tx.clone();
                let target = target.clone();
                // Never block a native callback: WinRT can dispatch it while Show is awaited.
                runtime.spawn(async move {
                    let _ = event_tx
                        .send(super::events::ClientLoopEvent::NotificationActivated(
                            target,
                        ))
                        .await;
                });
            }),
        )
    })
}

pub(super) fn handle_notify(
    kind: NotifyKind,
    message: &str,
    body: Option<&str>,
    sound_config: &crate::config::SoundConfig,
) {
    handle_notify_with_notifiers(
        kind,
        message,
        body,
        sound_config,
        crate::terminal_notify::show_notification,
        show_untargeted_system_notification,
    );
}

pub(super) fn handle_notify_with_notifiers(
    kind: NotifyKind,
    message: &str,
    body: Option<&str>,
    sound_config: &crate::config::SoundConfig,
    mut show_terminal_notification: impl FnMut(&str, Option<&str>) -> io::Result<bool>,
    mut show_system_notification: impl FnMut(&str, Option<&str>) -> io::Result<bool>,
) {
    match kind {
        NotifyKind::Sound => {
            let Some(sound) = sound_from_notify_message(message) else {
                warn!(
                    message = message,
                    "received unknown sound notification from server"
                );
                return;
            };
            if sound_config.enabled {
                if super::is_remote_or_ssh_session() {
                    let _ = crate::terminal_effects::write_terminal_bells(&mut io::stdout(), 1);
                } else {
                    crate::sound::play(sound, sound_config);
                }
            }
            if sound_config.visual_bell.unwrap_or(false) {
                let _ = crate::terminal_effects::write_visual_bell(&mut io::stdout());
            }
        }
        NotifyKind::Toast => {
            debug!(
                message = message,
                "received terminal toast notification from server"
            );
            if let Err(err) = show_terminal_notification(message, body) {
                warn!(err = %err, "failed to emit terminal notification");
            }
        }
        NotifyKind::SystemToast => {
            debug!(
                message = message,
                "received system toast notification from server"
            );
            if let Err(err) = show_system_notification(message, body) {
                warn!(err = %err, "failed to emit system notification");
            }
        }
    }
}

pub(super) fn sound_from_notify_message(message: &str) -> Option<crate::sound::Sound> {
    match message {
        "agent done" => Some(crate::sound::Sound::Done),
        "agent attention" => Some(crate::sound::Sound::Request),
        _ => None,
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[tokio::test(flavor = "current_thread")]
    async fn native_delivery_keeps_client_responsive_and_preserves_replacement_order() {
        let (started, ready) = tokio::sync::oneshot::channel();
        let (release, wait) = std::sync::mpsc::channel();
        queue_system_notification(move || {
            let _ = started.send(());
            wait.recv_timeout(std::time::Duration::from_secs(5))
                .map_err(io::Error::other)?;
            Ok(true)
        })
        .expect("queued first notification");
        ready.await.expect("notification worker started");
        let (finished, mut second) = tokio::sync::oneshot::channel();
        queue_system_notification(move || {
            let _ = finished.send(());
            Ok(true)
        })
        .expect("queued replacement");
        tokio::task::yield_now().await;
        assert_eq!(
            second.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        );
        // This send must run while native delivery is still waiting.
        release.send(()).expect("client loop remained responsive");
        second.await.expect("replacement delivered after original");
    }
}

#[cfg(test)]
mod route_tests {
    use super::{
        rootshell_route_pane, terminal_notification_route, TerminalNotificationContext as Context,
        TerminalNotificationRoute as Route,
    };
    use crate::client::endpoint::{ClientEndpointId, ProfileId};
    use crate::client::shell::{ClientNotificationActivation, ClientSystemNotificationTarget};

    #[test]
    fn rootshell_pushes_route_only_to_local_panes() {
        let activation = |endpoint_id| ClientNotificationActivation {
            token: "token".into(),
            target: ClientSystemNotificationTarget {
                endpoint_id,
                boot_id: "boot".into(),
                pane_id: "w1:p2".into(),
            },
        };
        assert_eq!(
            rootshell_route_pane(&activation(ClientEndpointId::Local)).as_deref(),
            Some("w1:p2")
        );
        let remote = ClientEndpointId::Ssh(ProfileId::generate());
        assert_eq!(rootshell_route_pane(&activation(remote)), None);
    }

    fn targeted() -> Context {
        Context {
            has_target: true,
            ..Context::default()
        }
    }

    #[test]
    fn kitty_reports_its_own_clicks_everywhere() {
        for remote in [false, true] {
            let context = Context {
                terminal_reports_clicks: true,
                terminal_shows_notifications: true,
                remote,
                desktop_reports_clicks: true,
                rootshell: remote,
                ..targeted()
            };
            assert_eq!(
                terminal_notification_route(context),
                Route::ReportingTerminal
            );
        }
    }

    #[test]
    fn terminals_that_show_notifications_keep_their_own_toast() {
        for remote in [false, true] {
            let context = Context {
                terminal_shows_notifications: true,
                remote,
                desktop_reports_clicks: true,
                ..targeted()
            };
            assert_eq!(terminal_notification_route(context), Route::Terminal);
        }
    }

    #[test]
    fn rootshell_sessions_push_to_the_device() {
        let context = Context {
            terminal_shows_notifications: true,
            remote: true,
            desktop_reports_clicks: true,
            rootshell: true,
            ..targeted()
        };
        assert_eq!(terminal_notification_route(context), Route::Rootshell);
        let plain_ssh = Context {
            rootshell: false,
            ..context
        };
        assert_eq!(terminal_notification_route(plain_ssh), Route::Terminal);
    }

    #[test]
    fn other_local_terminals_use_a_clickable_desktop_notifier() {
        let context = Context {
            desktop_reports_clicks: true,
            ..targeted()
        };
        assert_eq!(terminal_notification_route(context), Route::Desktop);
        assert_eq!(terminal_notification_route(targeted()), Route::Terminal);
    }

    #[test]
    fn untargeted_notifications_keep_the_terminal_escape_outside_windows() {
        let context = Context {
            desktop_reports_clicks: true,
            ..Context::default()
        };
        let route = terminal_notification_route(context);
        if cfg!(windows) {
            assert_eq!(route, Route::Desktop);
        } else {
            assert_eq!(route, Route::Terminal);
        }
    }
}
