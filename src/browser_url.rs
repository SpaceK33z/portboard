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
        .map(|endpoint| listener_browser_url(&endpoint.address, endpoint.port))
}

/// Copies a URL to the terminal clipboard with OSC 52 (base64-encoded).
///
/// Returns false when the sequence cannot be written so the caller can tell
/// the user to select the visible URL text instead. OSC 52 needs no helper
/// binary and works across SSH because the terminal that owns the pane does
/// the copy, so it is the right choice for a panel that can run remotely.
pub fn copy_url_to_clipboard(url: &str) -> bool {
    if validate_browser_url(url).is_err() {
        return false;
    }
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine;
    let encoded = STANDARD.encode(url.as_bytes());
    let sequence = format!("\x1b]52;c;{encoded}\x07");
    use std::io::Write;
    let mut stdout = std::io::stdout();
    if stdout.write_all(sequence.as_bytes()).is_err() {
        return false;
    }
    stdout.flush().is_ok()
}

/// Wraps a URL in an OSC 8 terminal hyperlink.
///
/// Herdr ctrl-click opens both OSC 8 hyperlinks and visible http(s) URLs, and
/// that click is handled by the local client, so the link keeps working when
/// the pane lives on a remote server behind `herdr --remote`.
pub fn osc8_hyperlink(url: &str) -> String {
    if validate_browser_url(url).is_err() {
        return "[invalid URL]".to_string();
    }
    format!("\x1b]8;;{url}\x1b\\{url}\x1b]8;;\x1b\\")
}

/// Opens the URL in a browser when Portboard runs on a local desktop.
///
/// SSH sessions (including panes on a remote Herdr server reached through
/// `herdr --remote`) never spawn a server-side browser; callers should present
/// the URL as a clickable link instead so the local client can open it.
pub fn open_browser_url(url: &str) -> UrlOpenOutcome {
    if validate_browser_url(url).is_err() {
        return UrlOpenOutcome::OfferedLink;
    }
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

/// Validate the raw value before URL parsing can strip or encode controls.
pub fn validate_browser_url(value: &str) -> anyhow::Result<url::Url> {
    anyhow::ensure!(
        !value.chars().any(char::is_control),
        "Portboard URL contains control characters"
    );
    let parsed = url::Url::parse(value).map_err(|_| anyhow::anyhow!("Portboard URL is invalid"))?;
    anyhow::ensure!(
        matches!(parsed.scheme(), "http" | "https") && parsed.host().is_some(),
        "Portboard URL requires HTTP or HTTPS and a host"
    );
    Ok(parsed)
}

/// Convert a kernel TCP bind address into a browser authority, consistently.
pub fn listener_browser_url(address: &str, port: u16) -> String {
    let address = address.trim_matches(['[', ']']);
    let host = match address {
        "0.0.0.0" => "127.0.0.1",
        "::" => "::1",
        other => other,
    };
    if host.contains(':') {
        format!("http://[{host}]:{port}")
    } else {
        format!("http://{host}:{port}")
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
    fn rejects_terminal_control_urls() {
        let malicious = "http://localhost:3000/\x07\x1b]52;c;AAAA\x07";
        assert!(!osc8_hyperlink(malicious).contains('\x07'));
        assert!(!osc8_hyperlink(malicious).contains('\x1b'));
    }

    #[test]
    fn ipv6_and_wildcard_listeners_are_browser_ready() {
        for (address, expected) in [
            ("::1", "http://[::1]:4100"),
            ("::", "http://[::1]:4100"),
            ("0.0.0.0", "http://127.0.0.1:4100"),
        ] {
            assert_eq!(
                browser_url(&stopped_status_with_listeners(&[(address, 4100)])).as_deref(),
                Some(expected)
            );
        }
    }

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
            status: None,
        }
    }
}
