/// Where an inspector or browser remote-debugging server is listening.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InspectEndpoint {
    /// Hostname or IP, without brackets.
    pub host: String,
    /// TCP port.
    pub port: u16,
    /// Set when the user passed a `ws://` debugger URL. Discovery is skipped.
    pub websocket_url: Option<String>,
}

/// Parse `--inspect` values: a port, `host:port`, `http://` URL, or `ws://` URL.
pub fn parse_inspect(input: &str) -> Result<InspectEndpoint, String> {
    let input = input.trim();
    if input.is_empty() {
        return Err("inspect endpoint is empty".to_string());
    }
    if input.starts_with("wss://") || input.starts_with("https://") {
        return Err(
            "only ws:// and http:// inspector endpoints are supported (the inspector is local)"
                .to_string(),
        );
    }
    if let Some(rest) = input.strip_prefix("ws://") {
        let hostport = rest.split('/').next().unwrap_or(rest);
        let (host, port) = split_host_port(hostport, None)?;
        return Ok(InspectEndpoint {
            host,
            port,
            websocket_url: Some(input.to_string()),
        });
    }
    if let Some(rest) = input.strip_prefix("http://") {
        let hostport = rest.split('/').next().unwrap_or(rest);
        let (host, port) = split_host_port(hostport, Some(80))?;
        return Ok(InspectEndpoint {
            host,
            port,
            websocket_url: None,
        });
    }
    if input.chars().all(|ch| ch.is_ascii_digit()) {
        let port = parse_port(input)?;
        return Ok(InspectEndpoint {
            host: "127.0.0.1".to_string(),
            port,
            websocket_url: None,
        });
    }
    let (host, port) = split_host_port(input, None)?;
    Ok(InspectEndpoint {
        host,
        port,
        websocket_url: None,
    })
}

fn split_host_port(hostport: &str, default_port: Option<u16>) -> Result<(String, u16), String> {
    if hostport.is_empty() {
        return Err("inspect endpoint is missing a host".to_string());
    }
    if let Some((host, port)) = hostport.rsplit_once(':')
        && !host.is_empty()
        && !host.contains(':')
    {
        return Ok((host.to_string(), parse_port(port)?));
    }
    if let Some(port) = default_port {
        return Ok((hostport.to_string(), port));
    }
    Err(format!("expected a port or host:port, got `{hostport}`"))
}

fn parse_port(text: &str) -> Result<u16, String> {
    let port: u16 = text.parse().map_err(|_| format!("invalid port `{text}`"))?;
    if port == 0 {
        return Err("port 0 is invalid".to_string());
    }
    Ok(port)
}

#[cfg(test)]
mod tests {
    use super::parse_inspect;

    #[test]
    fn parses_ports_hosts_and_urls() {
        let port = parse_inspect("9229").unwrap();
        assert_eq!(port.host, "127.0.0.1");
        assert_eq!(port.port, 9229);
        assert!(port.websocket_url.is_none());

        let host = parse_inspect("localhost:9230").unwrap();
        assert_eq!(host.host, "localhost");
        assert_eq!(host.port, 9230);

        let http = parse_inspect("http://127.0.0.1:9222/json/list").unwrap();
        assert_eq!(http.port, 9222);
        assert!(http.websocket_url.is_none());

        let ws = parse_inspect("ws://127.0.0.1:9229/devtools/page/abc").unwrap();
        assert_eq!(
            ws.websocket_url.as_deref(),
            Some("ws://127.0.0.1:9229/devtools/page/abc")
        );
        assert_eq!(ws.port, 9229);
    }

    #[test]
    fn rejects_tls_and_empty_values() {
        assert!(parse_inspect("").is_err());
        assert!(parse_inspect("wss://127.0.0.1:9229/x").is_err());
        assert!(parse_inspect("0").is_err());
        assert!(parse_inspect("localhost").is_err());
    }
}
