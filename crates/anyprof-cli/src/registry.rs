use anyprof_core::Registry;
use anyprof_go::GoBackend;
use anyprof_native::NativeBackend;
use anyprof_node::NodeBackend;

/// Language backends first, native last. Native matches any live pid, so it is
/// the fallback when the process is not Node.js or Go.
pub fn default_registry() -> Registry {
    Registry::new(vec![
        Box::new(NodeBackend),
        Box::new(GoBackend),
        Box::new(NativeBackend),
    ])
}

#[cfg(test)]
mod tests {
    use super::default_registry;

    #[test]
    fn detection_order_is_node_then_go_then_native() {
        assert_eq!(default_registry().names(), vec!["node", "go", "native"]);
    }
}
