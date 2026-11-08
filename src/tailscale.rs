use crate::state::SharedState;
use serde_json::Value;
use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::path::Path;
use std::time::Duration;
use tokio::process::Command;
use tokio::time::timeout;

const TAILSCALE_COMMAND_TIMEOUT: Duration = Duration::from_secs(60);
const FUNNEL_HTTPS_PORTS: [u16; 3] = [443, 10000, 8443];

fn tailscale_binary() -> OsString {
    if let Some(explicit) =
        std::env::var_os("CATDESK_TAILSCALE_BIN").filter(|path| !path.is_empty())
    {
        return explicit;
    }
    // On macOS the GUI (App Store/Standalone) daemon does not reliably
    // support public Funnel ingress. Only the open-source CLI daemon is used.
    #[cfg(target_os = "macos")]
    {
        for candidate in [
            "/opt/homebrew/opt/tailscale/bin/tailscale",
            "/usr/local/opt/tailscale/bin/tailscale",
        ] {
            if Path::new(candidate).is_file() {
                return OsString::from(candidate);
            }
        }
    }
    OsString::from("tailscale")
}

fn socket_argument(path: &OsStr) -> Result<OsString, String> {
    if !Path::new(path).is_absolute() {
        return Err("CATDESK_TAILSCALE_SOCKET must be an absolute path".into());
    }
    let mut flag = OsString::from("--socket=");
    flag.push(path);
    Ok(flag)
}

fn configured_socket_argument() -> Result<Option<OsString>, String> {
    match std::env::var_os("CATDESK_TAILSCALE_SOCKET").filter(|path| !path.is_empty()) {
        Some(path) => socket_argument(&path).map(Some),
        None => {
            #[cfg(target_os = "macos")]
            {
                Err("macOS Funnel requires a separate open-source tailscaled in userspace mode; configure CATDESK_TAILSCALE_SOCKET".into())
            }
            #[cfg(not(target_os = "macos"))]
            {
                Ok(None)
            }
        }
    }
}

async fn run_tailscale(args: &[&str]) -> Result<Vec<u8>, String> {
    let mut command = Command::new(tailscale_binary());
    if let Some(socket) = configured_socket_argument()? {
        command.arg(socket);
    }
    let output = timeout(
        TAILSCALE_COMMAND_TIMEOUT,
        command.env("TAILSCALE_BE_CLI", "1").args(args).output(),
    )
    .await
    .map_err(|_| "Tailscale CLI timed out; check the Tailscale daemon and login".to_string())?
    .map_err(|error| {
        format!("Cannot start Tailscale CLI: {error}. Install and sign in to Tailscale first")
    })?;

    if !output.status.success() {
        // Avoid logging CLI stdout/stderr because it may include tailnet metadata.
        return Err(format!(
            "Tailscale CLI failed (exit code {:?}) for '{}'. Check the CLI manually",
            output.status.code(),
            args.first().copied().unwrap_or("status"),
        ));
    }
    Ok(output.stdout)
}

fn public_origin_from_status(bytes: &[u8]) -> Result<String, String> {
    let status: Value = serde_json::from_slice(bytes)
        .map_err(|_| "Tailscale status did not return valid JSON".to_string())?;
    if status.get("BackendState").and_then(Value::as_str) != Some("Running") {
        return Err("Tailscale is not connected; run 'tailscale up' first".into());
    }

    let dns_name = status
        .get("Self")
        .and_then(|node| node.get("DNSName"))
        .and_then(Value::as_str)
        .ok_or_else(|| "Tailscale MagicDNS hostname is unavailable".to_string())?;
    let hostname = dns_name.trim_end_matches('.').to_ascii_lowercase();
    if !hostname.ends_with(".ts.net")
        || hostname.split('.').count() < 4
        || !hostname.split('.').all(|label| {
            !label.is_empty()
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        })
    {
        return Err("Tailscale returned an invalid *.ts.net hostname".into());
    }
    Ok(format!("https://{hostname}"))
}

fn advertised_port(host_and_port: &str) -> Option<u16> {
    host_and_port.rsplit_once(':')?.1.parse().ok()
}

fn select_safe_funnel_port(bytes: &[u8], target: &str) -> Result<u16, String> {
    let config: Value = serde_json::from_slice(bytes)
        .map_err(|_| "Tailscale Funnel status did not return valid JSON".to_string())?;
    let mut occupied = BTreeSet::new();
    let mut owned = BTreeSet::new();

    if let Some(tcp) = config.get("TCP").and_then(Value::as_object) {
        for key in tcp.keys() {
            if let Ok(port) = key.parse::<u16>() {
                occupied.insert(port);
            }
        }
    }

    if let Some(web) = config.get("Web").and_then(Value::as_object) {
        for (host_and_port, settings) in web {
            let Some(port) = advertised_port(host_and_port) else {
                return Err("Unrecognized Tailscale Serve hostname/port in existing config".into());
            };
            occupied.insert(port);

            // Only reuse a previously configured Funnel when its single root
            // handler is exactly CatDesk. Never turn another Serve port public.
            let handlers = settings.get("Handlers").and_then(Value::as_object);
            let exact_target = handlers.is_some_and(|handlers| {
                handlers.len() == 1
                    && handlers
                        .get("/")
                        .and_then(|handler| handler.get("Proxy"))
                        .and_then(Value::as_str)
                        == Some(target)
            });
            let already_public = config
                .get("AllowFunnel")
                .and_then(Value::as_object)
                .is_some_and(|flags| {
                    flags.iter().any(|(key, allowed)| {
                        (key == host_and_port || advertised_port(key) == Some(port))
                            && allowed.as_bool() == Some(true)
                    })
                });
            if exact_target && already_public {
                owned.insert(port);
            }
        }
    }

    for port in FUNNEL_HTTPS_PORTS {
        if owned.contains(&port) {
            return Ok(port);
        }
    }
    for port in FUNNEL_HTTPS_PORTS {
        if !occupied.contains(&port) {
            return Ok(port);
        }
    }
    Err("All Tailscale Funnel HTTPS ports (443, 10000, 8443) are occupied. Existing routes were not modified".into())
}

/// Configures a public Funnel mapping using an unused public HTTPS port.
/// The existing Tailscale Serve/Funnel routes are preserved, and the MCP
/// router stays bound to loopback.
pub async fn start(state: SharedState) -> Result<(), String> {
    let (port, mcp_path) = {
        let app = state.lock().await;
        if app.tunnel_running {
            return Err("A public tunnel is already configured".into());
        }
        (app.port, app.mcp_path())
    };

    let status = run_tailscale(&["status", "--json"]).await?;
    let origin = public_origin_from_status(&status)?;
    let target = format!("http://127.0.0.1:{port}");
    let existing = run_tailscale(&["funnel", "status", "--json"]).await?;
    let public_port = select_safe_funnel_port(&existing, &target)?;
    let public_port_flag = format!("--https={public_port}");

    run_tailscale(&["funnel", "--bg", &public_port_flag, &target])
        .await
        .map_err(|error| {
            format!("{error}. Funnel requires HTTPS, MagicDNS and the funnel node attribute")
        })?;

    // A CLI success confirms configuration, not end-to-end reachability.
    let origin = if public_port == 443 {
        origin
    } else {
        format!("{origin}:{public_port}")
    };
    let mut app = state.lock().await;
    app.tunnel_running = true;
    app.tunnel_url = Some(origin.clone());
    app.log(
        "INFO",
        "Tailscale Funnel configured (external connectivity not yet verified)".into(),
    );
    app.log("INFO", format!("MCP Server URL: {origin}{mcp_path}"));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{public_origin_from_status, select_safe_funnel_port, socket_argument};
    use std::ffi::OsStr;

    #[test]
    fn isolated_socket_requires_absolute_path() {
        assert!(socket_argument(OsStr::new("relative/daemon.sock")).is_err());
    }

    #[test]
    fn isolated_socket_is_passed_as_an_explicit_cli_argument() {
        assert_eq!(
            socket_argument(OsStr::new("/tmp/catdesk-tailscale/daemon.sock")),
            Ok("--socket=/tmp/catdesk-tailscale/daemon.sock".into()),
        );
    }

    #[test]
    fn derives_url_from_running_status() {
        let status = br#"{"BackendState":"Running","Self":{"DNSName":"mac.tailabc123.ts.net."}}"#;
        assert_eq!(
            public_origin_from_status(status).as_deref(),
            Ok("https://mac.tailabc123.ts.net")
        );
    }

    #[test]
    fn refuses_disconnected_tailnet() {
        let status = br#"{"BackendState":"Stopped","Self":{"DNSName":"mac.tailabc123.ts.net."}}"#;
        assert!(public_origin_from_status(status).is_err());
    }

    #[test]
    fn refuses_non_tailscale_hostname() {
        let status = br#"{"BackendState":"Running","Self":{"DNSName":"public.example.com."}}"#;
        assert!(public_origin_from_status(status).is_err());
    }

    #[test]
    fn refuses_hostname_with_injection_chars() {
        let status =
            br#"{"BackendState":"Running","Self":{"DNSName":"mac;rm.tailabc123.ts.net."}}"#;
        assert!(public_origin_from_status(status).is_err());
    }

    #[test]
    fn refuses_status_with_no_self_node() {
        let status = br#"{"BackendState":"Running"}"#;
        assert!(public_origin_from_status(status).is_err());
    }

    #[test]
    fn selects_443_when_all_ports_are_free() {
        assert_eq!(
            select_safe_funnel_port(b"{}", "http://127.0.0.1:3200"),
            Ok(443)
        );
    }

    #[test]
    fn preserves_occupied_443_and_8443_and_selects_10000() {
        let config = br#"{"TCP":{"443":{"HTTPS":true},"8443":{"HTTPS":true}},"Web":{"host.ts.net:443":{"Handlers":{"/":{"Proxy":"http://127.0.0.1:5000"}}},"host.ts.net:8443":{"Handlers":{"/":{"Proxy":"http://127.0.0.1:5273"}}}}}"#;
        assert_eq!(
            select_safe_funnel_port(config, "http://127.0.0.1:3200"),
            Ok(10000)
        );
    }

    #[test]
    fn refuses_overwriting_any_preexisting_route() {
        let config = br#"{"TCP":{"443":{},"8443":{},"10000":{}},"Web":{}}"#;
        assert!(select_safe_funnel_port(config, "http://127.0.0.1:3200").is_err());
    }

    #[test]
    fn reuses_only_exact_own_existing_public_route() {
        let config = br#"{"TCP":{"443":{},"8443":{},"10000":{}},"Web":{"host.ts.net:10000":{"Handlers":{"/":{"Proxy":"http://127.0.0.1:3200"}}}},"AllowFunnel":{"host.ts.net:10000":true}}"#;
        assert_eq!(
            select_safe_funnel_port(config, "http://127.0.0.1:3200"),
            Ok(10000)
        );
    }

    #[test]
    fn refuses_making_existing_private_route_public() {
        let config = br#"{"TCP":{"443":{},"8443":{},"10000":{}},"Web":{"host.ts.net:10000":{"Handlers":{"/":{"Proxy":"http://127.0.0.1:3200"}}}}}"#;
        assert!(select_safe_funnel_port(config, "http://127.0.0.1:3200").is_err());
    }

    #[test]
    fn fails_closed_on_malformed_status_json() {
        assert!(select_safe_funnel_port(b"not JSON", "http://127.0.0.1:3200").is_err());
    }
}
