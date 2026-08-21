use std::env;
use std::process::{Command, Stdio};

use crate::current_workspace_status::CurrentLaunchTargetStatus;

/// What happened when Portboard tried to open a browser URL.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UrlOpenOutcome {
    /// A local desktop browser was launched on this machine.
    OpenedLocally,
    /// This machine is reached over SSH (including `herdr --remote`), so no
    /// server-side browser was started; the URL is offered as a clickable link.
    OfferedLink,
}

/// Returns the best browser-ready URL for a launch target status.
///
/// Preference order: the primary named runtime endpoint, the first named
/// endpoint, then the first discovered TCP listener as an HTTP URL.
pub fn browser_url(status: &CurrentLaunchTargetStatus) -> Option<String> {
    if let Some(primary) = status
        .named_endpoints
        .iter()
        .find(|endpoint| endpoint.primary)
    {
        return Some(primary.url.clone());
    }
    if let Some(named) = status.named_endpoints.first() {
        return Some(named.url.clone());
    }
    status
        .endpoints
        .first()
        .map(|endpoint| format!("http://{}:{}", endpoint.address, endpoint.port))
}

/// Wraps a URL in an OSC 8 terminal hyperlink.
///
/// Herdr ctrl-click opens both OSC 8 hyperlinks and visible http(s) URLs, and
/// that click is handled by the local client, so the link keeps working when
/// the pane lives on a remote server behind `herdr --remote`.
pub fn osc8_hyperlink(url: &str) -> String {
    format!("\x1b]8;;{url}\x1b\\{url}\x1b]8;;\x1b\\")
}

/// Opens the URL in a browser when Portboard runs on a local desktop.
///
/// SSH sessions (including panes on a remote Herdr server reached through
/// `herdr --remote`) never spawn a server-side browser; callers should present
/// the URL as a clickable link instead so the local client can open it.
pub fn open_browser_url(url: &str) -> UrlOpenOutcome {
    if running_over_ssh() {
        return UrlOpenOutcome::OfferedLink;
    }
    if env::var_os("DISPLAY").is_none() && env::var_os("WAYLAND_DISPLAY").is_none() {
        return UrlOpenOutcome::OfferedLink;
    }
    let spawned = Command::new("xdg-open")
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    match spawned {
        Ok(_) => UrlOpenOutcome::OpenedLocally,
        Err(_) => UrlOpenOutcome::OfferedLink,
    }
}

fn running_over_ssh() -> bool {
    ["SSH_CONNECTION", "SSH_TTY"]
        .iter()
        .any(|variable| env::var_os(variable).is_some_and(|value| !value.is_empty()))
}

#[cfg(test)]
mod tests {
    use crate::current_workspace_status::{
        inspect_worktree_launch_targets, LaunchTargetRuntimeState,
    };
    use crate::launch_target_config::parse_launch_target_config;

    use super::{browser_url, osc8_hyperlink};

    #[test]
    fn prefers_the_primary_named_endpoint() {
        let mut status = stopped_status_with_listeners(&[("127.0.0.1", 4100)]);
        status.named_endpoints = vec![
            named_endpoint("api", "http://localhost:4101", false),
            named_endpoint("web", "http://localhost:4102", true),
        ];

        assert_eq!(
            browser_url(&status).as_deref(),
            Some("http://localhost:4102")
        );
    }

    #[test]
    fn falls_back_to_the_first_named_endpoint_without_a_primary() {
        let mut status = stopped_status_with_listeners(&[("127.0.0.1", 4100)]);
        status.named_endpoints = vec![named_endpoint("web", "http://localhost:4103", false)];

        assert_eq!(
            browser_url(&status).as_deref(),
            Some("http://localhost:4103")
        );
    }

    #[test]
    fn falls_back_to_the_first_discovered_listener() {
        let status = stopped_status_with_listeners(&[("127.0.0.1", 4104), ("[::1]", 4105)]);

        assert_eq!(
            browser_url(&status).as_deref(),
            Some("http://127.0.0.1:4104")
        );
    }

    #[test]
    fn returns_no_url_without_endpoints() {
        let status = stopped_status_with_listeners(&[]);

        assert_eq!(browser_url(&status), None);
    }

    #[test]
    fn wraps_urls_in_an_osc8_hyperlink() {
        assert_eq!(
            osc8_hyperlink("http://localhost:4106"),
            "\x1b]8;;http://localhost:4106\x1b\\http://localhost:4106\x1b]8;;\x1b\\"
        );
    }

    fn stopped_status_with_listeners(
        listeners: &[(&str, u16)],
    ) -> crate::current_workspace_status::CurrentLaunchTargetStatus {
        let temporary = tempfile::tempdir().expect("temporary worktree");
        let config = parse_launch_target_config(
            r#"
version = 1

[[launch_targets]]
id = "probe"
label = "Probe"
argv = ["missing-command"]
process_match = ["portboard-browser-url-test"]
"#,
        )
        .expect("launch target config");
        let inspection =
            inspect_worktree_launch_targets(temporary.path(), &config).expect("statuses");
        let mut status = inspection.statuses.into_iter().next().expect("status");
        assert_eq!(status.state, LaunchTargetRuntimeState::Stopped);
        status.endpoints = listeners
            .iter()
            .map(
                |(address, port)| crate::process_endpoints::ProcessEndpoint {
                    protocol: "tcp",
                    address: (*address).to_string(),
                    port: *port,
                },
            )
            .collect();
        status
    }

    fn named_endpoint(
        id: &str,
        url: &str,
        primary: bool,
    ) -> crate::runtime_metadata::RuntimeEndpoint {
        crate::runtime_metadata::RuntimeEndpoint {
            id: id.to_string(),
            url: url.to_string(),
            primary,
        }
    }
}
