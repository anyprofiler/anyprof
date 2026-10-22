use serde::Deserialize;

use crate::endpoint::InspectEndpoint;
use crate::error::Error;
use crate::http::http_get;

/// One inspector target from `/json/list`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DebugTarget {
    /// Target id assigned by the inspector.
    pub id: String,
    /// Page or script title. Often empty for Node.
    pub title: String,
    /// Page URL or script URL.
    pub url: String,
    /// Inspector type: `node`, `page`, `iframe`, ...
    pub kind: String,
    /// WebSocket debugger URL.
    pub websocket_url: String,
}

/// GET `/json/list` (falling back to `/json`) and return targets that have a socket.
pub fn list_targets(endpoint: &InspectEndpoint) -> Result<Vec<DebugTarget>, Error> {
    let body = match http_get(&endpoint.host, endpoint.port, "/json/list") {
        Ok(body) => body,
        Err(err) => {
            tracing::debug!(%err, "json/list failed, trying /json");
            http_get(&endpoint.host, endpoint.port, "/json")?
        }
    };
    let raw: Vec<RawTarget> = serde_json::from_slice(&body).map_err(|err| {
        let snippet = String::from_utf8_lossy(&body);
        let snippet: String = snippet.chars().take(180).collect();
        Error::message(format!(
            "inspector target list is not JSON ({err}): {snippet}"
        ))
    })?;
    Ok(raw
        .into_iter()
        .filter_map(|target| {
            if target.websocket_url.is_empty() {
                None
            } else {
                Some(DebugTarget {
                    id: target.id,
                    title: target.title,
                    url: target.url,
                    kind: target.kind,
                    websocket_url: target.websocket_url,
                })
            }
        })
        .collect())
}

/// Pick the Node.js target. `hints` are script paths from the process command line.
///
/// A single Node target is used even when no hint matches. Several targets require
/// a unique hint match so anyprof does not profile the wrong process.
pub fn choose_node<'a>(
    targets: &'a [DebugTarget],
    hints: &[String],
) -> Result<&'a DebugTarget, Error> {
    let nodes: Vec<&DebugTarget> = targets
        .iter()
        .filter(|target| target.kind == "node")
        .collect();
    if nodes.is_empty() {
        return Err(Error::message(format!(
            "inspector has no node target ({})",
            describe(targets)
        )));
    }
    if let Some(matched) = unique_hint(&nodes, hints) {
        return Ok(matched);
    }
    if nodes.len() == 1 {
        return Ok(nodes[0]);
    }
    Err(Error::message(format!(
        "several node targets are listening; pass --inspect with the websocket URL ({})",
        describe(&owned(&nodes))
    )))
}

/// Pick a browser page by target id, or by a substring of the URL or title.
///
/// With no selector, the only open page is used. Several pages require `--tab`.
pub fn choose_page<'a>(
    targets: &'a [DebugTarget],
    selector: Option<&str>,
) -> Result<&'a DebugTarget, Error> {
    let pages: Vec<&DebugTarget> = targets
        .iter()
        .filter(|target| target.kind == "page")
        .collect();
    if pages.is_empty() {
        return Err(Error::message(format!(
            "no browser page targets ({})",
            describe(targets)
        )));
    }
    let Some(selector) = selector.map(str::trim).filter(|value| !value.is_empty()) else {
        if pages.len() == 1 {
            return Ok(pages[0]);
        }
        return Err(Error::message(format!(
            "several browser tabs are open; pass --tab with a url, title, or id ({})",
            describe(&owned(&pages))
        )));
    };
    let exact: Vec<&&DebugTarget> = pages
        .iter()
        .filter(|target| target.id == selector)
        .collect();
    if let [only] = exact.as_slice() {
        return Ok(*only);
    }
    let loose: Vec<&&DebugTarget> = pages
        .iter()
        .filter(|target| {
            target.url.contains(selector)
                || target.title.contains(selector)
                || target.id.contains(selector)
        })
        .collect();
    match loose.as_slice() {
        [only] => Ok(*only),
        [] => Err(Error::message(format!(
            "no tab matched `{selector}` ({})",
            describe(&owned(&pages))
        ))),
        matched => Err(Error::message(format!(
            "`{selector}` matched {} tabs; pass a more specific --tab ({})",
            matched.len(),
            describe(&owned(&pages))
        ))),
    }
}

fn owned(targets: &[&DebugTarget]) -> Vec<DebugTarget> {
    targets.iter().map(|target| (*target).clone()).collect()
}

fn unique_hint<'a>(nodes: &[&'a DebugTarget], hints: &[String]) -> Option<&'a DebugTarget> {
    for hint in hints {
        if hint.is_empty() {
            continue;
        }
        let matched: Vec<&&DebugTarget> = nodes
            .iter()
            .filter(|target| {
                target.url.contains(hint.as_str()) || target.title.contains(hint.as_str())
            })
            .collect();
        if let [only] = matched.as_slice() {
            return Some(*only);
        }
    }
    None
}

fn describe(targets: &[DebugTarget]) -> String {
    if targets.is_empty() {
        return "none".to_string();
    }
    targets
        .iter()
        .map(|target| {
            let label = if target.url.is_empty() {
                target.title.as_str()
            } else {
                target.url.as_str()
            };
            format!("{} {label} ({})", target.kind, target.id)
        })
        .collect::<Vec<_>>()
        .join("; ")
}

#[derive(Deserialize)]
struct RawTarget {
    #[serde(default)]
    id: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    url: String,
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(rename = "webSocketDebuggerUrl", default)]
    websocket_url: String,
}

#[cfg(test)]
mod tests {
    use super::{DebugTarget, choose_node, choose_page};

    fn target(kind: &str, id: &str, url: &str, title: &str) -> DebugTarget {
        DebugTarget {
            id: id.to_string(),
            title: title.to_string(),
            url: url.to_string(),
            kind: kind.to_string(),
            websocket_url: format!("ws://127.0.0.1/{id}"),
        }
    }

    #[test]
    fn node_hint_picks_the_script_and_a_lone_target_needs_no_hint() {
        let targets = vec![
            target("node", "a", "file:///app/server.js", ""),
            target("node", "b", "file:///other.js", ""),
        ];
        let chosen = choose_node(&targets, &["server.js".to_string()]).unwrap();
        assert_eq!(chosen.id, "a");

        let only = vec![target("node", "a", "", "")];
        assert_eq!(choose_node(&only, &[]).unwrap().id, "a");
        assert!(choose_node(&targets, &[]).is_err());
    }

    #[test]
    fn page_selector_matches_url_title_or_id() {
        let targets = vec![
            target("page", "tab-1", "https://example.com/a", "Example"),
            target("page", "tab-2", "https://other.test/", "Other"),
            target("browser", "browser", "", ""),
        ];
        assert_eq!(
            choose_page(&targets, Some("example.com")).unwrap().id,
            "tab-1"
        );
        assert_eq!(choose_page(&targets, Some("Other")).unwrap().id, "tab-2");
        assert_eq!(choose_page(&targets, Some("tab-1")).unwrap().id, "tab-1");
        assert!(choose_page(&targets, None).is_err());
        assert!(choose_page(&targets, Some("missing")).is_err());
    }
}
