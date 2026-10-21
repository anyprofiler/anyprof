use std::path::PathBuf;
use std::time::Duration;

use clap::{ArgAction, Args, Parser, Subcommand, ValueEnum};

#[derive(Debug, Parser)]
#[command(
    name = "anyprof",
    version,
    about = "Capture CPU profiles from any running process, no code changes or restarts needed.",
    propagate_version = true,
    arg_required_else_help = true
)]
pub struct Cli {
    /// Increase log verbosity (`-v` info, `-vv` debug, `-vvv` trace).
    #[arg(short, long, action = ArgAction::Count, global = true)]
    pub verbose: u8,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Sample a running process and write a profile.
    Record(RecordArgs),
    /// Convert a profile between collapsed stacks, speedscope, pprof, and cpuprofile.
    Convert(ConvertArgs),
}

#[derive(Debug, Args)]
#[command(after_help = "\
Examples:
  anyprof record --pid 1234 --duration 30s -o cpu.collapsed
  anyprof record --pid 1234 --inspect 9229 --duration 10s --format speedscope -o cpu.json
  anyprof record --browser --tab example.com --duration 10s -o page.collapsed")]
pub struct RecordArgs {
    /// Process to profile. Required unless `--browser` is set.
    #[arg(
        long,
        value_name = "PID",
        value_parser = clap::value_parser!(u32).range(1..),
        required_unless_present = "browser"
    )]
    pub pid: Option<u32>,

    /// Profile a browser tab instead of a pid.
    ///
    /// Chrome, Chromium, or Edge must already be running with
    /// `--remote-debugging-port` (default 9222).
    #[arg(long)]
    pub browser: bool,

    /// How long to sample.
    #[arg(
        long,
        value_name = "DURATION",
        value_parser = humantime::parse_duration,
        default_value = "30s"
    )]
    pub duration: Duration,

    /// Output file.
    #[arg(short, long, value_name = "PATH")]
    pub output: PathBuf,

    /// Output format.
    #[arg(long, value_enum, default_value = "collapsed")]
    pub format: ProfileFormat,

    /// Sampling frequency in hertz.
    #[arg(long, default_value_t = 99, value_parser = clap::value_parser!(u32).range(1..))]
    pub frequency: u32,

    /// Include kernel frames when the backend supports them.
    #[arg(long)]
    pub kernel: bool,

    /// Force a backend (`node`, `browser`, `go`, or `native`) instead of auto-detecting.
    #[arg(long, value_name = "NAME")]
    pub backend: Option<String>,

    /// Inspector or remote-debugging endpoint: a port, `host:port`, `http://` URL, or `ws://` URL.
    ///
    /// Node defaults to the process's listening inspector port, then 9229.
    /// Browser defaults to `127.0.0.1:9222`.
    #[arg(long, value_name = "PORT_OR_URL")]
    pub inspect: Option<String>,

    /// Browser tab: target id, or a substring of the page URL or title. Requires `--browser`.
    #[arg(long, value_name = "URL_TITLE_OR_ID")]
    pub tab: Option<String>,
}

#[derive(Debug, Args)]
#[command(after_help = "\
Examples:
  anyprof convert -i cpu.collapsed -o cpu.json --to speedscope
  anyprof convert -i cpu.collapsed -o cpu.pb.gz --from collapsed --to pprof
  anyprof convert -i profile.cpuprofile -o cpu.collapsed")]
pub struct ConvertArgs {
    /// Input profile.
    #[arg(short, long, value_name = "PATH")]
    pub input: PathBuf,

    /// Output profile.
    #[arg(short, long, value_name = "PATH")]
    pub output: PathBuf,

    /// Input format. Inferred from the file name when omitted.
    #[arg(long, value_enum)]
    pub from: Option<ProfileFormat>,

    /// Output format.
    #[arg(long, value_enum)]
    pub to: ProfileFormat,
}

/// Encodings `record` and `convert` accept.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ProfileFormat {
    /// Folded stacks: `root;caller;leaf count`.
    Collapsed,
    /// gzip-compressed pprof protobuf.
    Pprof,
    /// speedscope JSON.
    Speedscope,
    /// V8 `.cpuprofile` JSON from Node.js or Chrome.
    Cpuprofile,
}

impl ProfileFormat {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Collapsed => "collapsed",
            Self::Pprof => "pprof",
            Self::Speedscope => "speedscope",
            Self::Cpuprofile => "cpuprofile",
        }
    }
}

impl From<ProfileFormat> for anyprof_export::Format {
    fn from(format: ProfileFormat) -> Self {
        match format {
            ProfileFormat::Collapsed => Self::Collapsed,
            ProfileFormat::Pprof => Self::Pprof,
            ProfileFormat::Speedscope => Self::Speedscope,
            ProfileFormat::Cpuprofile => Self::Cpuprofile,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Cli, Command, ProfileFormat};
    use clap::Parser;
    use std::time::Duration;

    #[test]
    fn record_defaults_and_humantime_duration() {
        let cli = Cli::try_parse_from([
            "anyprof",
            "record",
            "--pid",
            "42",
            "--duration",
            "1m30s",
            "-o",
            "cpu.collapsed",
        ])
        .unwrap();
        assert_eq!(cli.verbose, 0);
        let Command::Record(args) = cli.command else {
            panic!("expected record");
        };
        assert_eq!(args.pid, Some(42));
        assert!(!args.browser);
        assert_eq!(args.duration, Duration::from_secs(90));
        assert_eq!(args.output.as_os_str(), "cpu.collapsed");
        assert_eq!(args.format, ProfileFormat::Collapsed);
        assert_eq!(args.frequency, 99);
        assert!(!args.kernel);
        assert!(args.backend.is_none());
        assert!(args.inspect.is_none());
        assert!(args.tab.is_none());
    }

    #[test]
    fn record_accepts_format_backend_verbosity_and_kernel() {
        let cli = Cli::try_parse_from([
            "anyprof",
            "-vv",
            "record",
            "--pid",
            "7",
            "--format",
            "pprof",
            "--frequency",
            "199",
            "--kernel",
            "--backend",
            "native",
            "-o",
            "cpu.pb.gz",
        ])
        .unwrap();
        assert_eq!(cli.verbose, 2);
        let Command::Record(args) = cli.command else {
            panic!("expected record");
        };
        assert_eq!(args.format, ProfileFormat::Pprof);
        assert_eq!(args.frequency, 199);
        assert!(args.kernel);
        assert_eq!(args.backend.as_deref(), Some("native"));
        assert_eq!(args.duration, Duration::from_secs(30));
    }

    #[test]
    fn verbosity_can_follow_the_subcommand() {
        let cli = Cli::try_parse_from([
            "anyprof",
            "convert",
            "-vvv",
            "-i",
            "in.collapsed",
            "-o",
            "out.json",
            "--to",
            "speedscope",
        ])
        .unwrap();
        assert_eq!(cli.verbose, 3);
        let Command::Convert(args) = cli.command else {
            panic!("expected convert");
        };
        assert!(args.from.is_none());
        assert_eq!(args.to, ProfileFormat::Speedscope);
    }

    #[test]
    fn rejects_missing_pid_zero_pid_and_unknown_format() {
        assert!(Cli::try_parse_from(["anyprof", "record", "-o", "out"]).is_err());
        assert!(Cli::try_parse_from(["anyprof", "record", "--pid", "0", "-o", "out"]).is_err());
        assert!(Cli::try_parse_from([
            "anyprof",
            "record",
            "--pid",
            "1",
            "--frequency",
            "0",
            "-o",
            "out"
        ])
        .is_err());
        assert!(Cli::try_parse_from([
            "anyprof",
            "record",
            "--pid",
            "1",
            "--format",
            "flamegraph",
            "-o",
            "out"
        ])
        .is_err());
    }

    #[test]
    fn browser_record_does_not_require_a_pid() {
        let cli = Cli::try_parse_from([
            "anyprof",
            "record",
            "--browser",
            "--tab",
            "example.com",
            "--inspect",
            "9222",
            "-o",
            "page.collapsed",
        ])
        .unwrap();
        let Command::Record(args) = cli.command else {
            panic!("expected record");
        };
        assert!(args.browser);
        assert!(args.pid.is_none());
        assert_eq!(args.tab.as_deref(), Some("example.com"));
        assert_eq!(args.inspect.as_deref(), Some("9222"));
    }
}
