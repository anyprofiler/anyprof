use std::path::{Path, PathBuf};
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
    #[command(visible_alias = "rec")]
    Record(RecordArgs),
    /// Convert a profile between collapsed stacks, speedscope, pprof, and cpuprofile.
    #[command(visible_alias = "conv")]
    Convert(ConvertArgs),
    /// Upgrade this install to the latest GitHub release.
    ///
    /// Works for copies installed by the shell or PowerShell release installer.
    /// A `cargo install` build has no install receipt, so this command tells you
    /// to reinstall with that installer. Releases always come from
    /// github.com/anyprofiler/anyprof. Once the installer starts, Ctrl+C waits
    /// for it to exit, so on Windows the old anyprof.exe is restored if the
    /// install fails.
    #[command(
        visible_alias = "upgrade",
        alias = "uptate",
        after_help = "\
Examples:
  anyprof update
  anyprof upgrade"
    )]
    Update,
}

#[derive(Debug, Args)]
#[command(after_help = "\
Examples:
  anyprof rec -p 1234 -d 10s -o cpu.cpuprofile
  anyprof rec -p 1234
  anyprof rec -p 1234 -d 30s -f collapsed -o cpu.collapsed
  anyprof rec -p 1234 -i 9229 -d 10s -f speedscope -o cpu.json
  anyprof rec -p 1234 -d 5m --interval 1ms
  anyprof rec -B -t example.com -d 10s -o page.cpuprofile")]
pub struct RecordArgs {
    /// Process to profile. Required unless `--browser` is set.
    #[arg(
        short = 'p',
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
    #[arg(short = 'B', long)]
    pub browser: bool,

    /// How long to sample, for example `10s` or `1m30s`.
    ///
    /// Omit to record until Ctrl+C. Ctrl+C or SIGTERM stops sampling and still
    /// writes the profile. A second Ctrl+C while the profile is saved exits
    /// immediately.
    #[arg(
        short = 'd',
        long,
        value_name = "DURATION",
        value_parser = humantime::parse_duration
    )]
    pub duration: Option<Duration>,

    /// Output file or directory.
    ///
    /// Omit to write an auto-named file in the current directory. A directory,
    /// `.`, `..`, or a path ending in a separator gets an auto-named file inside
    /// it. `~` expands to the home directory. Auto names look like
    /// `anyprof-node-1234-20261022-122730.cpuprofile` and gain `-2`, `-3`, ...
    /// before the extension when that name already exists.
    #[arg(short, long, value_name = "PATH")]
    pub output: Option<PathBuf>,

    /// Output format. An explicit value wins.
    ///
    /// Otherwise a file path's extension is used (`.cpuprofile`, `.pb.gz`,
    /// `.pprof`, `.speedscope.json`, `.collapsed`, `.folded`, `.txt`). A
    /// directory, or no `-o`, uses the backend default: cpuprofile for Node and
    /// browser, pprof for Go and native.
    #[arg(short = 'f', long, value_enum)]
    pub format: Option<ProfileFormat>,

    /// Samples per second, for example `1000`.
    ///
    /// Defaults depend on the backend: Node and browser sample every 100us
    /// (10000 Hz), the same rate as a Chrome DevTools recording, and Go and
    /// native sample at 99 Hz. Conflicts with `--interval`.
    #[arg(
        short = 'F',
        long,
        value_name = "HZ",
        value_parser = parse_frequency
    )]
    pub frequency: Option<u32>,

    /// Time between samples, for example `100us`, `1ms`, or `10ms`.
    ///
    /// An alternative to `--frequency`. Node and browser round to whole
    /// microseconds.
    #[arg(
        long,
        value_name = "DURATION",
        value_parser = humantime::parse_duration,
        conflicts_with = "frequency"
    )]
    pub interval: Option<Duration>,

    /// Include kernel frames when the backend supports them.
    #[arg(short = 'k', long)]
    pub kernel: bool,

    /// Force a backend (`node`, `browser`, `go`, or `native`) instead of auto-detecting.
    #[arg(short = 'b', long, value_name = "NAME")]
    pub backend: Option<String>,

    /// Inspector or remote-debugging endpoint: a port, `host:port`, `http://` URL, or `ws://` URL.
    ///
    /// Node defaults to the process's listening inspector port, then 9229.
    /// Browser defaults to `127.0.0.1:9222`.
    #[arg(short = 'i', long, value_name = "PORT_OR_URL")]
    pub inspect: Option<String>,

    /// Browser tab: target id, or a substring of the page URL or title. Requires `--browser`.
    #[arg(short = 't', long, value_name = "URL_TITLE_OR_ID")]
    pub tab: Option<String>,
}

#[derive(Debug, Args)]
#[command(after_help = "\
Examples:
  anyprof conv -i cpu.collapsed -o cpu.json -t speedscope
  anyprof conv -i cpu.collapsed -o cpu.pb.gz -f collapsed -t pprof
  anyprof conv -i profile.cpuprofile -o cpu.collapsed -t collapsed")]
pub struct ConvertArgs {
    /// Input profile.
    #[arg(short = 'i', long, value_name = "PATH")]
    pub input: PathBuf,

    /// Output profile.
    #[arg(short, long, value_name = "PATH")]
    pub output: PathBuf,

    /// Input format. Inferred from the file name when omitted.
    #[arg(short = 'f', long, value_enum)]
    pub from: Option<ProfileFormat>,

    /// Output format.
    #[arg(short = 't', long, value_enum)]
    pub to: ProfileFormat,
}

/// One sample per microsecond. V8 takes whole-microsecond intervals, so a
/// faster `-F` could not be honored.
pub const MAX_FREQUENCY_HZ: u32 = 1_000_000;

fn parse_frequency(raw: &str) -> Result<u32, String> {
    let hertz: u32 = raw
        .parse()
        .map_err(|_| format!("expected a whole number of hertz, got '{raw}'"))?;
    if !(1..=MAX_FREQUENCY_HZ).contains(&hertz) {
        return Err(format!(
            "must be between 1 and {MAX_FREQUENCY_HZ} Hz (one sample per microsecond is the finest rate)"
        ));
    }
    Ok(hertz)
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

    pub(crate) fn extension(self) -> &'static str {
        anyprof_core::Format::from(self).extension()
    }
}

/// Infer a profile format from a file name.
///
/// `.speedscope.json` is checked before a bare `.json`. `.txt` is collapsed
/// stacks, the same as `.collapsed` and `.folded`.
pub(crate) fn infer_format(path: &Path) -> Option<ProfileFormat> {
    let name = path.file_name()?.to_string_lossy().to_ascii_lowercase();
    if name.ends_with(".speedscope.json") || name.ends_with(".speedscope") {
        return Some(ProfileFormat::Speedscope);
    }
    if name.ends_with(".pb.gz") || name.ends_with(".pprof") || name.ends_with(".pprof.gz") {
        return Some(ProfileFormat::Pprof);
    }
    if name.ends_with(".cpuprofile") {
        return Some(ProfileFormat::Cpuprofile);
    }
    match Path::new(&name).extension().and_then(|ext| ext.to_str())? {
        "collapsed" | "folded" | "txt" => Some(ProfileFormat::Collapsed),
        "json" => Some(ProfileFormat::Speedscope),
        _ => None,
    }
}

impl From<ProfileFormat> for anyprof_core::Format {
    fn from(format: ProfileFormat) -> Self {
        match format {
            ProfileFormat::Collapsed => Self::Collapsed,
            ProfileFormat::Pprof => Self::Pprof,
            ProfileFormat::Speedscope => Self::Speedscope,
            ProfileFormat::Cpuprofile => Self::Cpuprofile,
        }
    }
}

impl From<anyprof_core::Format> for ProfileFormat {
    fn from(format: anyprof_core::Format) -> Self {
        match format {
            anyprof_core::Format::Collapsed => Self::Collapsed,
            anyprof_core::Format::Pprof => Self::Pprof,
            anyprof_core::Format::Speedscope => Self::Speedscope,
            anyprof_core::Format::Cpuprofile => Self::Cpuprofile,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Cli, Command, ProfileFormat, infer_format};
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
        assert_eq!(args.duration, Some(Duration::from_secs(90)));
        assert_eq!(args.output.as_deref().unwrap().as_os_str(), "cpu.collapsed");
        assert_eq!(args.format, None);
        assert_eq!(
            infer_format(std::path::Path::new("cpu.cpuprofile")),
            Some(ProfileFormat::Cpuprofile)
        );
        assert_eq!(
            infer_format(std::path::Path::new("cpu.collapsed")),
            Some(ProfileFormat::Collapsed)
        );
        assert_eq!(
            infer_format(std::path::Path::new("cpu.folded")),
            Some(ProfileFormat::Collapsed)
        );
        assert_eq!(
            infer_format(std::path::Path::new("stacks.txt")),
            Some(ProfileFormat::Collapsed)
        );
        assert_eq!(
            infer_format(std::path::Path::new("cpu.pb.gz")),
            Some(ProfileFormat::Pprof)
        );
        assert_eq!(
            infer_format(std::path::Path::new("cpu.pprof")),
            Some(ProfileFormat::Pprof)
        );
        assert_eq!(
            infer_format(std::path::Path::new("cpu.speedscope.json")),
            Some(ProfileFormat::Speedscope)
        );
        assert_eq!(infer_format(std::path::Path::new("cpu")), None);
        assert_eq!(args.frequency, None);
        assert_eq!(args.interval, None);
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
        assert_eq!(args.format, Some(ProfileFormat::Pprof));
        assert_eq!(args.frequency, Some(199));
        assert!(args.kernel);
        assert_eq!(args.backend.as_deref(), Some("native"));
        assert_eq!(args.duration, None);
        assert!(args.output.is_some());
    }

    #[test]
    fn omitted_output_and_duration_record_until_ctrl_c() {
        let cli = Cli::try_parse_from(["anyprof", "record", "-p", "3"]).unwrap();
        let Command::Record(args) = cli.command else {
            panic!("expected record");
        };
        assert_eq!(args.pid, Some(3));
        assert!(args.output.is_none());
        assert!(args.duration.is_none());
        assert_eq!(args.frequency, None);
        assert_eq!(args.interval, None);
    }

    #[test]
    fn interval_takes_a_duration_and_conflicts_with_frequency() {
        for (raw, expected) in [
            ("100us", Duration::from_micros(100)),
            ("1ms", Duration::from_millis(1)),
            ("10ms", Duration::from_millis(10)),
        ] {
            let cli =
                Cli::try_parse_from(["anyprof", "rec", "-p", "3", "--interval", raw]).unwrap();
            let Command::Record(args) = cli.command else {
                panic!("expected record");
            };
            assert_eq!(args.interval, Some(expected), "{raw}");
            assert_eq!(args.frequency, None);
        }

        let err = Cli::try_parse_from([
            "anyprof",
            "rec",
            "-p",
            "3",
            "-F",
            "1000",
            "--interval",
            "1ms",
        ])
        .unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
        assert!(Cli::try_parse_from(["anyprof", "rec", "-p", "3", "--interval", "fast"]).is_err());
    }

    #[test]
    fn frequency_is_capped_at_one_sample_per_microsecond() {
        let cli = Cli::try_parse_from(["anyprof", "rec", "-p", "3", "-F", "1000000"]).unwrap();
        let Command::Record(args) = cli.command else {
            panic!("expected record");
        };
        assert_eq!(args.frequency, Some(1_000_000));

        for raw in ["1000001", "4000000000"] {
            let err = Cli::try_parse_from(["anyprof", "rec", "-p", "3", "-F", raw]).unwrap_err();
            assert_eq!(err.kind(), clap::error::ErrorKind::ValueValidation, "{raw}");
            let message = err.to_string();
            assert!(message.contains("between 1 and 1000000 Hz"), "{message}");
        }
        let err = Cli::try_parse_from(["anyprof", "rec", "-p", "3", "-F", "fast"]).unwrap_err();
        assert!(err.to_string().contains("whole number of hertz"), "{err}");
    }

    #[test]
    fn record_short_flags_and_alias() {
        let cli = Cli::try_parse_from([
            "anyprof",
            "-v",
            "rec",
            "-p",
            "42",
            "-d",
            "10s",
            "-f",
            "cpuprofile",
            "-F",
            "50",
            "-b",
            "node",
            "-i",
            "9229",
            "-k",
            "-o",
            "out.cpuprofile",
        ])
        .unwrap();
        assert_eq!(cli.verbose, 1);
        let Command::Record(args) = cli.command else {
            panic!("expected record");
        };
        assert_eq!(args.pid, Some(42));
        assert_eq!(args.duration, Some(Duration::from_secs(10)));
        assert_eq!(args.format, Some(ProfileFormat::Cpuprofile));
        assert_eq!(args.frequency, Some(50));
        assert_eq!(args.backend.as_deref(), Some("node"));
        assert_eq!(args.inspect.as_deref(), Some("9229"));
        assert!(args.kernel);
        assert_eq!(
            args.output.as_deref().unwrap().as_os_str(),
            "out.cpuprofile"
        );
    }

    #[test]
    fn browser_short_flag_does_not_take_the_backend_short_flag() {
        let cli = Cli::try_parse_from(["anyprof", "rec", "-B", "-t", "example.com", "-i", "9222"])
            .unwrap();
        let Command::Record(args) = cli.command else {
            panic!("expected record");
        };
        assert!(args.browser);
        assert!(args.pid.is_none());
        assert_eq!(args.tab.as_deref(), Some("example.com"));
        assert_eq!(args.inspect.as_deref(), Some("9222"));
        assert!(args.backend.is_none());
        assert!(args.output.is_none());
        assert!(args.duration.is_none());
    }

    #[test]
    fn convert_short_flags_and_alias() {
        let cli = Cli::try_parse_from([
            "anyprof",
            "conv",
            "-i",
            "a.collapsed",
            "-o",
            "b.json",
            "-f",
            "collapsed",
            "-t",
            "speedscope",
        ])
        .unwrap();
        let Command::Convert(args) = cli.command else {
            panic!("expected convert");
        };
        assert_eq!(args.input.as_os_str(), "a.collapsed");
        assert_eq!(args.output.as_os_str(), "b.json");
        assert_eq!(args.from, Some(ProfileFormat::Collapsed));
        assert_eq!(args.to, ProfileFormat::Speedscope);
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
        assert!(
            Cli::try_parse_from([
                "anyprof",
                "record",
                "--pid",
                "1",
                "--frequency",
                "0",
                "-o",
                "out"
            ])
            .is_err()
        );
        assert!(
            Cli::try_parse_from([
                "anyprof",
                "record",
                "--pid",
                "1",
                "--format",
                "flamegraph",
                "-o",
                "out"
            ])
            .is_err()
        );
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

    #[test]
    fn update_aliases_parse() {
        for name in ["update", "upgrade", "uptate"] {
            let cli = Cli::try_parse_from(["anyprof", name]).unwrap();
            assert!(
                matches!(cli.command, Command::Update),
                "{name} should select update"
            );
        }
    }
}
