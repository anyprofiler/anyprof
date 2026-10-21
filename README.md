# anyprof

Capture CPU profiles from any running process, no code changes or restarts needed.

anyprof attaches to a live process by pid and writes a CPU profile. The process does not need a profiler baked in, and it does not need to be restarted. Runtimes (native code, Node.js, Go, and later others) sit behind one `Backend` trait, and every backend emits the same profile.

**Status:** this repository is the scaffold. The CLI, data model, auto-detection, and exporters build and run. Sampling itself is not implemented yet: `anyprof record` selects a backend and then exits with a clear "not implemented yet" error. `anyprof convert` works today.

## Goals

- Profile a running process by pid, without code changes or a restart.
- One data model for every runtime. A frame records which runtime it came from; file, line, module, and address are optional and may be empty.
- Backends declare what they can do (on-CPU, off-CPU, mixed stacks, kernel frames, whether they need root) instead of pretending they are identical.
- Write profiles in formats people already use: collapsed stacks, speedscope, and pprof.

## Runtime support

| Runtime | Crate | Status | Notes |
| --- | --- | --- | --- |
| C, C++, Rust | `anyprof-native` | Stub (MVP) | Linux on-CPU sampling is the first sampler to build. Detection treats any live pid as native, so this backend is the fallback. |
| Node.js | `anyprof-node` | Stub (MVP) | Planned path is the V8 inspector over the Chrome DevTools Protocol. Detection matches a process named `node` or `nodejs`. |
| Python | — | Planned | After the MVP. |
| Java | — | Planned | After the MVP. |
| Go | `anyprof-go` | Stub | Detection scans the executable for the Go build-info magic on Linux. Sampling comes after Python and Java. |
| Ruby | — | Planned | |
| PHP | — | Planned | |
| .NET | — | Planned | |
| macOS | — | Planned | The workspace builds on macOS. Samplers land after the Linux MVP. |

Auto-detection tries `node`, then `go`, then `native`. Pass `--backend` to skip detection.

## Architecture

```
anyprof record --pid <PID>
        |
        v
anyprof-cli -------- auto-detect / --backend
        |
        v
anyprof-core        Backend::detect / start -> Session::stop -> Profile
        ^
        |
        +-- anyprof-node     (Node.js, checked first)
        +-- anyprof-go       (Go)
        +-- anyprof-native   (fallback for any live pid)
        |
        v
anyprof-export       collapsed | speedscope | pprof
```

`Backend` is the extension point:

```rust
trait Backend {
    fn name(&self) -> &'static str;
    fn detect(&self, pid: u32) -> bool;
    fn capabilities(&self) -> Capabilities;
    fn start(&self, target: &Target, opts: &RecordOptions) -> Result<Box<dyn Session>, Error>;
}

trait Session {
    fn stop(self: Box<Self>) -> Result<Profile, Error>;
}
```

Stacks are leaf-first: `frames[0]` is the leaf (the frame that was on-CPU) and the last frame is the root. That matches pprof. Collapsed stacks and speedscope are root-first, and the exporters reverse the order.

A frame from a non-native runtime is written as `[runtime] name`, for example `[node] work`, so mixed stacks keep their origin in every format.

## Quick start

Requires Rust 1.85 or newer. CI uses Rust 1.99.

```bash
cargo build --release

# Show the CLI. Sampling is not implemented yet; this still selects a backend.
anyprof record --help
anyprof record --pid 1234 --duration 30s -o cpu.collapsed

# Convert a collapsed profile (this path works today).
anyprof convert -i cpu.collapsed -o cpu.speedscope.json --to speedscope
anyprof convert -i cpu.collapsed -o cpu.pb.gz --to pprof
```

`record` options:

| Flag | Meaning |
| --- | --- |
| `--pid <PID>` | Process to profile. Required. |
| `--duration <DURATION>` | How long to sample. Default `30s`. Accepts `humantime` values such as `90s` or `1m30s`. |
| `-o, --output <PATH>` | Output file. Required. |
| `--format <collapsed\|pprof\|speedscope>` | Output format. Default `collapsed`. |
| `--frequency <HZ>` | Requested sampling rate. Default `99`. |
| `--kernel` | Ask for kernel frames when the backend supports them. |
| `--backend <native\|node\|go>` | Skip auto-detection. |
| `-v`, `-vv`, `-vvv` | Log more (`info`, `debug`, `trace`). `RUST_LOG` is used when `-v` is not passed. |

`convert` reads collapsed stacks, speedscope JSON, and pprof (gzip-compressed or raw protobuf) and writes any of the three. `--from` is inferred from the file name (`.collapsed`, `.folded`, `.json`, `.speedscope.json`, `.pb.gz`, `.pprof`) when you omit it.

Collapsed stacks keep the runtime label and function name. Speedscope also keeps file, line, and column. pprof also keeps the instruction address, duration, and sampling period. A frame's module is not written yet.

Collapsed output is the folded format FlameGraph expects:

```
main;[node] work 3
[kernel] schedule;[go] runtime.main 4
```

## Permissions

Native sampling on Linux will need the same access as `perf`: often root or `CAP_PERFMON` / `CAP_SYS_PTRACE`, depending on `kernel.perf_event_paranoid`. The CLI only profiles the pid you pass it. Backends report `needs-root` in their capabilities when that is expected. No sampler attaches to a process yet.

## Roadmap

1. **MVP:** Linux native on-CPU sampling and Node.js via the V8 inspector, on x86_64 and arm64.
2. Python and Java.
3. Go sampling (detection already exists).
4. macOS samplers, then Ruby, PHP, and .NET.

## Development

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build --workspace
```

CI runs those checks on Ubuntu and macOS. `rust-toolchain.toml` pins Rust 1.99.1, which is the compiler CI uses, so `cargo fmt` and `cargo clippy` match. The crate MSRV is 1.85.

| Path | Role |
| --- | --- |
| `crates/anyprof-core` | `Backend`, `Session`, `Profile`, process inspection |
| `crates/anyprof-native` | Linux native backend (stub) |
| `crates/anyprof-node` | Node.js backend (stub) |
| `crates/anyprof-go` | Go backend (stub; Linux build-info detection works) |
| `crates/anyprof-export` | collapsed, speedscope, and pprof |
| `crates/anyprof-cli` | the `anyprof` binary |

Linux-only process inspection is behind `cfg(target_os = "linux")`. On other Unix systems, detection falls back to `ps` for the process name and does not read an executable.

## License

Apache-2.0. See [LICENSE](LICENSE).
