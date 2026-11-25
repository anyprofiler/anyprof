# Runtime & Unwinder Internals

This document captures the low-level sampling, stack-unwinding, symbol-resolution, and runtime ABI invariants enforced across `anyprof` backends. For installation, CLI usage, and quick-start examples, see the top-level [README](../README.md).

## Data Model & Frame Conventions

- **Leaf-first internal stacks**: `Profile::stacks` stores frames from `frames[0]` (on-CPU leaf) to the root frame, matching `pprof`. Exporters for root-first formats (`collapsed` and `speedscope`) reverse frame order on write.
- **Runtime prefixes**: Non-native frames carry a `[runtime] name` prefix (`[node]`, `[browser]`, `[rust]`, `[go]`, `[python]`, `[java]`, `[kernel]`) across `collapsed`, `speedscope`, `pprof`, and rebuilt `.cpuprofile` exports so mixed-language stacks preserve frame provenance. Raw V8 `.cpuprofile` captures from `Profiler.stop` are written verbatim.

## Platform Native Samplers (`anyprof-native`)

### macOS (`aarch64` & `x86_64`)

- **Attach & sampling**: Uses Mach `task_for_pid` (requires `root`; SIP and hardened-runtime targets are refused by the kernel). Refuses `anyprof`'s own PID before attach so self-suspension cannot deadlock.
- **Thread suspension & Compact Unwind**: Suspends only threads currently in the `TH_STATE_RUNNING` state, unwinds Mach-O Compact Unwind (`__unwind_info`) and DWARF `__eh_frame` tables via `framehop` (`MustNotAllocateDuringUnwind`, zero heap allocations while suspended) over 4 KiB page-cached `mach_vm_read_overwrite` stack reads with frame-pointer fallback, then resumes threads via RAII guards with retry; if `thread_resume` still fails, the profile is aborted rather than returning a partially wedged target.
- **Pointer authentication (`arm64` / `arm64e`)**: Follows Darwin's `__darwin_arm_thread_state64_ptrauth_strip` conventions. `NO_PTRAUTH` leaves register `pc`/`lr`/`sp`/`fp` untouched; `IB_SIGNED_LR` preserves `lr`; otherwise `xpaci` strips code pointers and `xpacd` strips data pointers (including saved frame records when an `arm64` binary calls into `arm64e` system frameworks) without trapping on invalid signatures.
- **Symbols & `dyld_shared_cache`**: Parses standalone Mach-O symbol tables (capped at 2,000,000 symbols and 64 MiB string tables), includes `/usr/lib/dyld` via `dyldImageLoadAddress`, and resolves Apple system dylibs (`MH_DYLIB_IN_CACHE`, bounded to `__TEXT` so shared `__LINKEDIT` never causes range overlap) directly from the active `dyld_v1` shared cache, its V1/V2 subcaches, `LC_DYLD_EXPORTS_TRIE`, and optional `.symbols` local-symbol files via positioned reads (skipping `"<redacted>"` placeholders).

### Linux (`x86_64` & `aarch64`)

- **Attach & sampling**: Opens per-thread `perf_event_open` ring buffers using `PERF_COUNT_SW_TASK_CLOCK` (strictly on-CPU; target threads are never suspended). Requests `BP/SP/IP` on `x86_64` and `FP (x29)/LR (x30)/SP/PC` on `aarch64`. Self-profiling is supported.
- **Unwinding**: Unwinds DWARF `.eh_frame` / `.eh_frame_hdr` / `.debug_frame` (including sectionless binaries via `PT_GNU_EH_FRAME`) offline using `framehop` over the kernel-captured `PERF_SAMPLE_STACK_USER` snapshot (8 KiB, stripping PAC-RET signatures via `xpaclri` / `hint #7` on `aarch64`), stitching any remaining outer frames from the kernel's user callchain when DWARF unwinding reaches the end of the 8 KiB stack copy, and falling back to frame-pointer + `LR` walking when DWARF tables are absent. Non-leaf return addresses subtract 1 byte before symbol lookup.
- **Container / namespace resolution**: Mapped binaries and shared libraries in `/proc/<pid>/maps` are opened via `/proc/<pid>/root/...` (rejecting `..` path traversal) while keeping clean host-independent module basenames. Load biases respect runtime OS page size (`4K`, `16K`, or `64K` on `aarch64`).
- **Symbols & kernel frames**: Reads ELF `.symtab` then `.dynsym` (up to 4,096 mapped images, including ELF64 extended `e_shnum == 0` section tables) and demangles Itanium C++ and Rust symbols. `-k` / `--kernel` enables kernel callchains and resolves `/proc/kallsyms` when `kernel.perf_event_paranoid <= 1` and `kernel.kptr_restrict == 0`.

### Windows (`x86_64`)

- **Attach & sampling**: Opens targets with `PROCESS_QUERY_INFORMATION | PROCESS_VM_READ` (attempting `SeDebugPrivilege` once on `ERROR_ACCESS_DENIED` for cross-user/elevated processes; protected processes remain off-limits). Refuses 32-bit and native ARM64 processes via `IsWow64Process2`, while allowing x64 processes running under ARM64 emulation. Refuses `anyprof`'s own PID before `OpenProcess`.
- **Thread suspension & unwinding**: Queries `NtQuerySystemInformation` to suspend only `Running` threads (skipping `Ready`/`Waiting`), walks stacks via `GetThreadContext` + `StackWalk64` using PE `.pdata` unwind tables, and guarantees `ResumeThread` on `Drop`. Never calls `SetThreadContext` or `WriteProcessMemory`.
- **Symbols**: Queries `dbghelp` (`SymFromAddr`, local image directory only; symbol servers and `_NT_SYMBOL_PATH` are never contacted). When `SymFromAddr` yields no symbol, parses local `.pdb` files (up to 1 GiB) referenced by the PE CodeView debug directory or sibling `.pdb`, verifying matching GUID and DBI/PDB age.

## Language Runtime Samplers

### Node.js & Browser (`anyprof-node`, `anyprof-browser`, `anyprof-cdp`)

- **Activation**: On Unix, `anyprof-node` verifies the target executable is `node`/`nodejs` before sending `SIGUSR1` (since `SIGUSR1` terminates non-Node processes by default). On Windows, runs `node -e "process._debugProcess(<pid>)"`.
- **Port & PID verification**: Discovers existing inspector listeners via `/proc/<pid>/net/tcp` (Linux), `lsof` (macOS), or `netstat` (Windows) before falling back to `127.0.0.1:9229`, then evaluates `process.pid` over CDP and aborts if the inspector belongs to a different process.
- **Cleanup**: When `anyprof` activates the inspector itself, it closes the inspector listener via `inspector.close()` before disconnecting (compatible with both CommonJS and ESM processes) unless `--keep-inspector` is set. Pre-existing `--inspect` listeners are left open.

### Rust (`anyprof-rust`)

- **Detection**: Scans up to the first 32 MiB of the target executable without attaching, matching ELF `.comment` (`rustc version `), standard-library `Option::unwrap`/`Result::unwrap` panic markers, `fatal runtime error:`, or `/rustc/<40-hex>/library/`.
- **Demangling**: Demangles Rust v0 (`_R...`) and legacy (`_ZN...17h<16-hex>E`, stripping crate hashes) symbols via `rustc-demangle` prior to C++ demangling, handling Mach-O's extra leading underscore.

### Go (`anyprof-go`)

- **Detection**: Scans the executable for `\xff Go buildinf:`.
- **Unwinding (`pclntab`)**: Parses Go 1.18+ / 1.20+ `pclntab` (`0xFFFFFFF0` / `0xFFFFFFF1`) from `.gopclntab`, `__gopclntab`, or PE `.rdata`. Supports both Go 1.18–1.25 header `textStart` and Go 1.26+ zero `textStart` resolved via `moduledata.text` (`.go.module` / `__go_module` / `.data`), including cgo non-zero text offsets and externally linked Mach-O chained fixups. Unwinds on-CPU goroutines via `pcsp` deltas (`SP + pcsp` on `x86_64`, `LR` / `SP` on `aarch64`), surviving `-ldflags="-s -w"`.

### Python (`anyprof-python`)

- **Supported builds**: Default GIL builds of 64-bit CPython **3.11**, **3.12**, and **3.13** (including embedded interpreters and subinterpreters). Older CPython, 3.14+, 32-bit images, and free-threaded (`Py_GIL_DISABLED`) builds are explicitly detected and refused rather than mis-walked.
- **Frame-chain walk**: Locates `_PyRuntime` (using validated header offsets for 3.11 and 3.12, and `_Py_DebugOffsets` with `xdebugpy` cookie for 3.13), matches the sampled OS thread via `native_thread_id`, and walks `current_frame -> previous`, resolving `co_qualname`, `co_filename`, and PEP 626 `co_linetable` source lines.
- **Hybrid attribution**: Leaves inside the CPython binary (including C helpers called from `_PyEval_EvalFrameDefault`) resolve to `[python]` frames; leaves in external C/C++/Rust extensions or `libc` retain their native stacks. On Linux, `process_vm_readv` permission is probed up front so `kernel.yama.ptrace_scope=1` against a non-child process fails fast with a clear `EPERM` diagnostic instead of silently emitting eval-loop C frames.

### Java (`anyprof-java`)

- **Supported JVMs**: 64-bit HotSpot (**JDK 17, 21, and 25** on `x86_64` and `aarch64`). Non-HotSpot VMs, 32-bit JVMs, debug `GrowableArrayBase` layouts, or mismatched `VMStructs` offsets are refused.
- **Serviceability & CodeCache walk**: Reads `gHotSpotVMStructs`, `gHotSpotVMTypes`, and `gHotSpotVMIntConstants` without loading a JVMTI agent or sending signals. Resolves compiled `nmethod` frames via `CodeHeap` (`HeapBlock` 16B/`_used@8` on JDK 17/21; 8B/`_used@4` and `_code_offset`/`_data_offset` on JDK 25) and advances callers via `sp + frame_size` (working even without `-XX:+PreserveFramePointer`). Resolves `Interpreter` frames via `sender_sp = -1` and `last_sp = -2` (`method` at `last_sp - 1`).
- **Torn-stack prevention (Linux)**: Stack-resident slots during HotSpot unwinding read from the kernel-captured `PERF_SAMPLE_STACK_USER` snapshot first, falling back to `process_vm_readv` only for heap/metaspace/code-cache metadata (`Method*`, `ConstMethod*`, `ConstantPool*`, `Symbol*`), while `poll()` wakes the consumer immediately on perf ring events.
