use std::fmt;
use std::time::Duration;

/// Runtime that produced a stack frame.
///
/// A single [`super::Stack`] may mix several runtimes. Native and kernel frames
/// use their own variants so exporters can keep them distinct from language frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Runtime {
    /// C, C++, Rust, or other native user-space code.
    #[default]
    Native,
    /// Kernel frame.
    Kernel,
    /// Go.
    Go,
    /// Node.js / V8.
    Node,
    /// Python.
    Python,
    /// Java (HotSpot or another JVM).
    Java,
    /// Ruby.
    Ruby,
    /// PHP.
    Php,
    /// .NET / CoreCLR.
    Dotnet,
    /// The backend could not tell which runtime owned the frame.
    Unknown,
}

impl Runtime {
    /// Every variant, in a stable order.
    pub const ALL: [Self; 10] = [
        Self::Native,
        Self::Kernel,
        Self::Go,
        Self::Node,
        Self::Python,
        Self::Java,
        Self::Ruby,
        Self::Php,
        Self::Dotnet,
        Self::Unknown,
    ];

    /// Stable short name used in exported profiles (`node`, `go`, `kernel`, ...).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Native => "native",
            Self::Kernel => "kernel",
            Self::Go => "go",
            Self::Node => "node",
            Self::Python => "python",
            Self::Java => "java",
            Self::Ruby => "ruby",
            Self::Php => "php",
            Self::Dotnet => "dotnet",
            Self::Unknown => "unknown",
        }
    }

    /// Parse a name produced by [`Self::as_str`].
    #[must_use]
    pub fn parse(label: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|runtime| runtime.as_str() == label)
    }
}

impl fmt::Display for Runtime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One frame in a sample.
///
/// Optional fields stay empty when the backend does not know them. Native
/// samples often have an address and no source line; language samples often
/// have a file and line and no address.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Frame {
    /// Runtime that owns this frame.
    pub runtime: Runtime,
    /// Function or symbol name. Empty when the backend only has an address.
    pub name: String,
    /// Source file, when symbolization produced one.
    pub file: Option<String>,
    /// 1-based source line.
    pub line: Option<u32>,
    /// 1-based source column.
    pub column: Option<u32>,
    /// Module, shared object, package, or jar that contained the frame.
    pub module: Option<String>,
    /// Instruction pointer, for native and kernel frames.
    pub address: Option<u64>,
}

impl Frame {
    /// A named frame with every optional field empty.
    #[must_use]
    pub fn new(runtime: Runtime, name: impl Into<String>) -> Self {
        Self {
            runtime,
            name: name.into(),
            file: None,
            line: None,
            column: None,
            module: None,
            address: None,
        }
    }
}

/// One unique call stack and how many samples hit it.
///
/// `frames` is leaf-first: index 0 is the leaf, and the last element is the root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stack {
    /// Frames from leaf to root.
    pub frames: Vec<Frame>,
    /// Number of samples that observed this exact stack.
    pub weight: u64,
}

/// A finished CPU profile.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Profile {
    /// Profiled process. `0` when the profile was converted from a format that
    /// does not record a pid.
    pub pid: u32,
    /// Process name (`comm`), when known.
    pub comm: Option<String>,
    /// How long the sampler ran. Zero when the source format has no duration.
    pub duration: Duration,
    /// Time between samples, when the backend knows it.
    pub period: Option<Duration>,
    /// Observed stacks. Exporters may merge entries that look identical.
    pub stacks: Vec<Stack>,
}

impl Profile {
    /// Sum of [`Stack::weight`] across the profile.
    #[must_use]
    pub fn total_weight(&self) -> u64 {
        self.stacks
            .iter()
            .fold(0, |sum, stack| sum.saturating_add(stack.weight))
    }
}
