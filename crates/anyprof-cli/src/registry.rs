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
    use anyprof_browser::BrowserBackend;
    use anyprof_core::{Backend, Format};
    use anyprof_go::GoBackend;
    use anyprof_native::NativeBackend;
    use anyprof_node::NodeBackend;

    #[test]
    fn detection_order_is_node_browser_go_native() {
        assert_eq!(
            default_registry().names(),
            vec!["node", "browser", "go", "native"]
        );
    }

    #[test]
    fn each_backend_defaults_to_its_ecosystem_format() {
        assert_eq!(NodeBackend.default_format(), Format::Cpuprofile);
        assert_eq!(BrowserBackend.default_format(), Format::Cpuprofile);
        assert_eq!(GoBackend.default_format(), Format::Pprof);
        assert_eq!(NativeBackend.default_format(), Format::Pprof);
    }

    #[test]
    fn v8_backends_sample_like_devtools_and_perf_backends_at_99_hz() {
        use std::time::Duration;

        let devtools = Duration::from_micros(100);
        assert_eq!(NodeBackend.default_sampling_interval(), devtools);
        assert_eq!(BrowserBackend.default_sampling_interval(), devtools);
        assert_eq!(GoBackend.default_sampling_interval().as_micros(), 10_101);
        assert_eq!(
            NativeBackend.default_sampling_interval().as_micros(),
            10_101
        );
    }
}
