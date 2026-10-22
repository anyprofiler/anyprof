use anyprof_browser::BrowserBackend;
use anyprof_core::Registry;
use anyprof_go::GoBackend;
use anyprof_native::NativeBackend;
use anyprof_node::NodeBackend;

/// Node is the first-class pid backend. The browser backend is explicit
/// (`--browser`) and never matches a pid. Go is more specific than native, so
/// it stays ahead of the fallback even though sampling it is lower priority.
/// Native matches any live pid.
pub fn default_registry() -> Registry {
    Registry::new(vec![
        Box::new(NodeBackend),
        Box::new(BrowserBackend),
        Box::new(GoBackend),
        Box::new(NativeBackend),
    ])
}

#[cfg(test)]
mod tests {
    use super::default_registry;

    #[test]
    fn detection_order_is_node_browser_go_native() {
        assert_eq!(
            default_registry().names(),
            vec!["node", "browser", "go", "native"]
        );
    }
}
