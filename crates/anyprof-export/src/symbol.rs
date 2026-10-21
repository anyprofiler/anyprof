use anyprof_core::{Frame, Runtime};

/// Fold a frame into the single token shared by collapsed, speedscope, and pprof.
///
/// Native frames stay bare. Every other runtime is prefixed so mixed stacks
/// keep their origin: `[node] work`.
pub(crate) fn format_symbol(frame: &Frame) -> String {
    let base = if frame.name.is_empty() {
        match frame.address {
            Some(addr) => format!("0x{addr:x}"),
            None => "unknown".to_string(),
        }
    } else {
        frame.name.clone()
    };
    match frame.runtime {
        Runtime::Native => base,
        other => format!("[{other}] {base}"),
    }
}

/// Inverse of [`format_symbol`] for tokens this crate wrote.
///
/// A native function whose name itself starts with `[node] ` would be parsed as
/// a node frame. Backends should not produce names in that shape.
pub(crate) fn parse_symbol(token: &str) -> (Runtime, &str) {
    let Some(rest) = token.strip_prefix('[') else {
        return (Runtime::Native, token);
    };
    let Some((label, name)) = rest.split_once("] ") else {
        return (Runtime::Native, token);
    };
    match Runtime::parse(label) {
        Some(runtime) if runtime != Runtime::Native => (runtime, name),
        _ => (Runtime::Native, token),
    }
}

#[cfg(test)]
mod tests {
    use super::{format_symbol, parse_symbol};
    use anyprof_core::{Frame, Runtime};

    #[test]
    fn native_names_have_no_prefix() {
        let frame = Frame::new(Runtime::Native, "main");
        assert_eq!(format_symbol(&frame), "main");
        assert_eq!(parse_symbol("main"), (Runtime::Native, "main"));
    }

    #[test]
    fn other_runtimes_roundtrip_through_the_prefix() {
        let frame = Frame::new(Runtime::Node, "work");
        let token = format_symbol(&frame);
        assert_eq!(token, "[node] work");
        assert_eq!(parse_symbol(&token), (Runtime::Node, "work"));
    }

    #[test]
    fn empty_native_frame_uses_the_address() {
        let mut frame = Frame::new(Runtime::Native, "");
        frame.address = Some(0xab);
        assert_eq!(format_symbol(&frame), "0xab");
    }
}
