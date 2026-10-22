/// Profile encodings anyprof can read and write.
///
/// Each backend picks the format its ecosystem already opens. Node.js and
/// browser tabs use [`Format::Cpuprofile`] (Chrome DevTools and VS Code). Go
/// and native profiles use [`Format::Pprof`] (`go tool pprof` and the pprof web
/// UI). Future scripting runtimes (Python, Ruby, PHP, and the rest) should
/// default to [`Format::Speedscope`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// `root;caller;leaf count` lines, suitable for FlameGraph.
    Collapsed,
    /// gzip-compressed pprof protobuf (`profile.proto`).
    Pprof,
    /// speedscope sampled-profile JSON.
    Speedscope,
    /// V8 `.cpuprofile` JSON from Node.js or Chrome.
    Cpuprofile,
}

impl Format {
    /// Stable name used in errors and CLI output.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Collapsed => "collapsed",
            Self::Pprof => "pprof",
            Self::Speedscope => "speedscope",
            Self::Cpuprofile => "cpuprofile",
        }
    }

    /// Filename suffix for an auto-generated profile, including the dot.
    ///
    /// pprof is `.pb.gz` and speedscope is `.speedscope.json`, so callers that
    /// insert a collision suffix must treat this whole string as the extension.
    #[must_use]
    pub const fn extension(self) -> &'static str {
        match self {
            Self::Collapsed => ".collapsed",
            Self::Pprof => ".pb.gz",
            Self::Speedscope => ".speedscope.json",
            Self::Cpuprofile => ".cpuprofile",
        }
    }
}
