//! CPython stacks for threads the native sampler already caught on CPU.
//!
//! Every Python function runs inside `_PyEval_EvalFrameDefault`, so a
//! frame-pointer walk names the interpreter. This module reads the
//! interpreter frame chain instead: the leaf is that thread's current
//! Python frame, then `previous`. The frame is [`Runtime::Python`], with
//! `co_qualname`, `co_filename`, and a line from `co_linetable`.
//!
//! The leaf program counter has to fall inside the interpreter image (the
//! executable or `libpython` that exports `_PyRuntime`). C helpers in that
//! image, such as allocation the eval loop calls, stay attributed to the
//! Python function. A leaf in another image (libc, a ctypes extension)
//! keeps the native walk. A leaf inside the image does too when this read
//! returns no chain: the thread id is missing, a pointer is not plausible,
//! or a read is short. Those frames stay native and are not labeled Python.
//! A denied `process_vm_readv` does not reach that path. `prepare_python`
//! fails `start` before a profile exists. A thread blocked in `time.sleep`
//! is off CPU and is not recorded. Only the thread that holds the GIL runs
//! bytecode, so
//! two pure-Python threads show up as the GIL switches. Parked threads are
//! not walked.
//!
//! Layouts that are walked: the default GIL builds of 3.11, 3.12, and 3.13.
//! 3.11 and 3.12 use offsets checked against those headers (`current_frame`
//! is 8 in 3.11 because `_PyCFrame` starts with `use_tracing`, and compact
//! ASCII data starts at 48 because `PyASCIIObject` still has `wstr`). 3.13
//! reads `_Py_DebugOffsets` from the file when `free_threaded` is 0. Any
//! other version, a free-threaded build, or a table that does not match
//! that 3.13 layout is not walked. Selecting the python backend then fails
//! instead of recording native interpreter frames under that name. On Linux
//! the frame read is `process_vm_readv`, which needs ptrace attach
//! permission; a denied read is the same kind of error. 32-bit images are
//! not walked.

use std::path::{Path, PathBuf};

use anyprof_core::{Error, Frame, Runtime};

use crate::python_sym::{self, PythonImage};

const MAX_INTERPRETERS: usize = 32;
const MAX_THREADS: usize = 4096;
const MAX_PY_FRAMES: usize = 64;
const MAX_STRING: usize = 4096;
const MAX_LINETABLE: usize = 1024 * 1024;

/// One counted stack: native addresses, or a language frame chain.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub(crate) enum CountedStack {
    /// Leaf-first addresses. `kernel_len` leading entries are kernel PCs.
    Addresses { frames: Vec<u64>, kernel_len: u16 },
    /// Python frames, optionally after kernel PCs from the same sample.
    Python {
        kernel: Vec<u64>,
        frames: Vec<PyName>,
    },
    /// Java methods, optionally after kernel PCs from the same sample.
    ///
    /// Frames carry `Class.method` plus optional source file and line, leaf
    /// first. Kernel frames stay addresses until the profile is finished.
    Java {
        kernel: Vec<u64>,
        frames: Vec<crate::java::JavaFrame>,
    },
}

impl CountedStack {
    pub(crate) fn recordable(&self) -> bool {
        match self {
            Self::Addresses { frames, .. } => !frames.is_empty(),
            Self::Python { kernel, frames } => !kernel.is_empty() || !frames.is_empty(),
            Self::Java { kernel, frames } => !kernel.is_empty() || !frames.is_empty(),
        }
    }
}

/// A Python function the frame chain named.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub(crate) struct PyName {
    pub name: String,
    pub file: String,
    pub line: u32,
}

/// Offsets into the running interpreter. Pointer fields are byte offsets.
#[derive(Debug)]
struct Offsets {
    interpreters_head: u64,
    interp_next: u64,
    threads_head: u64,
    tstate_next: u64,
    /// When set, the current frame is `*(*tstate + cframe) + current_frame`.
    cframe: Option<u64>,
    current_frame: u64,
    native_thread_id: u64,
    frame_code: u64,
    frame_previous: u64,
    frame_instr: u64,
    code_filename: u64,
    code_qualname: u64,
    code_linetable: u64,
    code_firstlineno: u64,
    code_adaptive: u64,
    str_state: u64,
    str_length: u64,
    /// Compact ASCII data. 48 on 3.11 (`wstr` is still in `PyASCIIObject`),
    /// 40 on 3.12 and 3.13.
    str_ascii: u64,
    /// Compact non-ASCII data. 72 on 3.11, 56 on 3.12 and 3.13.
    str_compact: u64,
    bytes_size: u64,
    bytes_data: u64,
}

/// A CPython image whose frame layout we can read.
pub(crate) struct PythonVm {
    runtime: u64,
    image_start: u64,
    image_end: u64,
    offsets: Offsets,
}

impl PythonVm {
    /// Python frames for `tid` when `pc` is inside the interpreter image.
    ///
    /// `read` copies remote bytes. `None` means the leaf is outside the
    /// image, or it is inside and the chain cannot be read (missing thread
    /// id, a pointer that is not plausible, or a short read). The caller
    /// keeps the native stack either way. This does not panic and does not
    /// invent a Python name.
    pub(crate) fn frames(
        &self,
        tid: u64,
        pc: u64,
        read: &mut impl FnMut(u64, &mut [u8]) -> bool,
    ) -> Option<Vec<PyName>> {
        if tid == 0 || !self.in_image(pc) {
            return None;
        }
        let mut interp = read_ptr(
            read,
            self.runtime.checked_add(self.offsets.interpreters_head)?,
        )?;
        for _ in 0..MAX_INTERPRETERS {
            if !plausible_ptr(interp) {
                break;
            }
            if let Some(frames) = self.thread_frames(interp, tid, read) {
                return Some(frames);
            }
            let next = read_ptr(read, interp.checked_add(self.offsets.interp_next)?)?;
            if next == interp {
                break;
            }
            interp = next;
        }
        None
    }

    fn in_image(&self, pc: u64) -> bool {
        pc >= self.image_start && pc < self.image_end
    }

    fn thread_frames(
        &self,
        interp: u64,
        tid: u64,
        read: &mut impl FnMut(u64, &mut [u8]) -> bool,
    ) -> Option<Vec<PyName>> {
        let mut thread = read_ptr(read, interp.checked_add(self.offsets.threads_head)?)?;
        for _ in 0..MAX_THREADS {
            if !plausible_ptr(thread) {
                return None;
            }
            let native = read_native(read, thread.checked_add(self.offsets.native_thread_id)?)?;
            if native == tid {
                return self.chain(thread, read);
            }
            let next = read_ptr(read, thread.checked_add(self.offsets.tstate_next)?)?;
            if next == thread {
                return None;
            }
            thread = next;
        }
        None
    }

    fn chain(
        &self,
        thread: u64,
        read: &mut impl FnMut(u64, &mut [u8]) -> bool,
    ) -> Option<Vec<PyName>> {
        let mut frame = self.current_frame(thread, read)?;
        let mut out = Vec::new();
        for index in 0..MAX_PY_FRAMES {
            if !plausible_ptr(frame) {
                break;
            }
            match self.one_frame(frame, read) {
                Some(name) => out.push(name),
                None if index == 0 => return None,
                None => break,
            }
            let next = read_ptr(read, frame.checked_add(self.offsets.frame_previous)?)?;
            if next == frame {
                break;
            }
            frame = next;
        }
        (!out.is_empty()).then_some(out)
    }

    fn current_frame(
        &self,
        thread: u64,
        read: &mut impl FnMut(u64, &mut [u8]) -> bool,
    ) -> Option<u64> {
        if let Some(cframe) = self.offsets.cframe {
            let cframe = read_ptr(read, thread.checked_add(cframe)?)?;
            read_ptr(read, cframe.checked_add(self.offsets.current_frame)?)
        } else {
            read_ptr(read, thread.checked_add(self.offsets.current_frame)?)
        }
    }

    fn one_frame(
        &self,
        frame: u64,
        read: &mut impl FnMut(u64, &mut [u8]) -> bool,
    ) -> Option<PyName> {
        let code = read_ptr(read, frame.checked_add(self.offsets.frame_code)?)?;
        let name = read_unicode(
            read,
            code.checked_add(self.offsets.code_qualname)?,
            &self.offsets,
        )?;
        if name.is_empty() {
            return None;
        }
        let file = read_unicode(
            read,
            code.checked_add(self.offsets.code_filename)?,
            &self.offsets,
        )
        .unwrap_or_default();
        let line = self.line_of(code, frame, read).unwrap_or(0);
        Some(PyName { name, file, line })
    }

    fn line_of(
        &self,
        code: u64,
        frame: u64,
        read: &mut impl FnMut(u64, &mut [u8]) -> bool,
    ) -> Option<u32> {
        let first = read_i32(read, code.checked_add(self.offsets.code_firstlineno)?)?;
        let instr = read_u64(read, frame.checked_add(self.offsets.frame_instr)?)?;
        let adaptive = code.checked_add(self.offsets.code_adaptive)?;
        let addr = instr as i64 - adaptive as i64;
        let table = read_bytes(
            read,
            code.checked_add(self.offsets.code_linetable)?,
            &self.offsets,
        );
        let Some(table) = table else {
            return positive_line(first);
        };
        Some(addr2line(&table, first, addr).unwrap_or(0)).filter(|line| *line > 0)
    }
}

/// Turns Python names into profile frames. Kernel frames stay with the caller.
pub(crate) fn python_profile_frames(names: &[PyName]) -> Vec<Frame> {
    names
        .iter()
        .map(|name| {
            let mut frame = Frame::new(Runtime::Python, name.name.clone());
            if !name.file.is_empty() {
                frame.file = Some(name.file.clone());
            }
            if name.line > 0 {
                frame.line = Some(name.line);
            }
            frame
        })
        .collect()
}

fn positive_line(line: i32) -> Option<u32> {
    u32::try_from(line).ok().filter(|line| *line > 0)
}

/// PEP 626 line table. `addr` is a byte offset from `co_code_adaptive`.
fn addr2line(table: &[u8], firstlineno: i32, addr: i64) -> Option<u32> {
    if addr < 0 {
        return positive_line(firstlineno);
    }
    let mut cursor = 0usize;
    let mut end = 0i64;
    let mut computed = firstlineno;
    let mut line = -1i32;
    while end <= addr {
        if cursor >= table.len() {
            return positive_line(firstlineno);
        }
        let byte = table[cursor];
        if byte & 128 == 0 {
            return positive_line(firstlineno);
        }
        computed = computed.saturating_add(line_delta(&table[cursor..]));
        line = if byte >> 3 == 0x1f { -1 } else { computed };
        end += i64::from((byte & 7) + 1) * 2;
        cursor += 1;
        while cursor < table.len() && table[cursor] & 128 == 0 {
            cursor += 1;
        }
    }
    if line <= 0 {
        positive_line(firstlineno)
    } else {
        positive_line(line)
    }
}

fn line_delta(entry: &[u8]) -> i32 {
    let Some(byte) = entry.first().copied() else {
        return 0;
    };
    let code = (byte >> 3) & 15;
    match code {
        13 | 14 => signed_varint(&entry[1..]),
        11 => 1,
        12 => 2,
        _ => 0,
    }
}

fn signed_varint(bytes: &[u8]) -> i32 {
    let mut cursor = 0usize;
    let mut value = 0u32;
    let mut shift = 0u32;
    loop {
        let Some(byte) = bytes.get(cursor).copied() else {
            return 0;
        };
        cursor += 1;
        value |= u32::from(byte & 63) << shift;
        if byte & 64 == 0 || shift >= 24 {
            break;
        }
        shift += 6;
    }
    if value & 1 == 1 {
        -i32::try_from(value >> 1).unwrap_or(0)
    } else {
        i32::try_from(value >> 1).unwrap_or(0)
    }
}

fn plausible_ptr(addr: u64) -> bool {
    (0x1000..crate::walk::USER_ADDR_LIMIT).contains(&addr) && addr.is_multiple_of(8)
}

fn read_u64(read: &mut impl FnMut(u64, &mut [u8]) -> bool, addr: u64) -> Option<u64> {
    if addr < 0x1000 {
        return None;
    }
    let mut buf = [0u8; 8];
    read(addr, &mut buf).then_some(u64::from_le_bytes(buf))
}

fn read_ptr(read: &mut impl FnMut(u64, &mut [u8]) -> bool, addr: u64) -> Option<u64> {
    let value = read_u64(read, addr)?;
    if value == 0 {
        return Some(0);
    }
    plausible_ptr(value).then_some(value)
}

fn read_i32(read: &mut impl FnMut(u64, &mut [u8]) -> bool, addr: u64) -> Option<i32> {
    if addr < 0x1000 {
        return None;
    }
    let mut buf = [0u8; 4];
    read(addr, &mut buf).then_some(i32::from_le_bytes(buf))
}

fn read_native(read: &mut impl FnMut(u64, &mut [u8]) -> bool, addr: u64) -> Option<u64> {
    if addr < 0x1000 {
        return None;
    }
    #[cfg(windows)]
    {
        let mut buf = [0u8; 4];
        read(addr, &mut buf).then_some(u64::from(u32::from_le_bytes(buf)))
    }
    #[cfg(not(windows))]
    {
        read_u64(read, addr)
    }
}

fn read_bytes(
    read: &mut impl FnMut(u64, &mut [u8]) -> bool,
    object: u64,
    offsets: &Offsets,
) -> Option<Vec<u8>> {
    let object = read_ptr(read, object)?;
    if !plausible_ptr(object) {
        return None;
    }
    let size = read_u64(read, object.checked_add(offsets.bytes_size)?)?;
    let size = usize::try_from(size).ok()?;
    if size == 0 || size > MAX_LINETABLE {
        return None;
    }
    let mut buf = vec![0u8; size];
    let data = object.checked_add(offsets.bytes_data)?;
    read(data, &mut buf).then_some(buf)
}

fn read_unicode(
    read: &mut impl FnMut(u64, &mut [u8]) -> bool,
    object: u64,
    offsets: &Offsets,
) -> Option<String> {
    let object = read_ptr(read, object)?;
    if !plausible_ptr(object) {
        return None;
    }
    let length = read_u64(read, object.checked_add(offsets.str_length)?)?;
    let length = usize::try_from(length).ok()?;
    if length == 0 || length > MAX_STRING {
        return None;
    }
    let mut state = [0u8; 1];
    if !read(object.checked_add(offsets.str_state)?, &mut state) {
        return None;
    }
    let kind = (state[0] >> 2) & 7;
    let compact = (state[0] >> 5) & 1 == 1;
    let ascii = (state[0] >> 6) & 1 == 1;
    if !compact {
        return None;
    }
    let (width, data_off) = if ascii && kind == 1 {
        (1usize, offsets.str_ascii)
    } else if kind == 1 || kind == 2 || kind == 4 {
        (usize::from(kind), offsets.str_compact)
    } else {
        return None;
    };
    let nbytes = length.checked_mul(width)?;
    if nbytes > MAX_STRING * 4 {
        return None;
    }
    let mut raw = vec![0u8; nbytes];
    if !read(object.checked_add(data_off)?, &mut raw) {
        return None;
    }
    decode_unicode(kind, &raw)
}

fn decode_unicode(kind: u8, raw: &[u8]) -> Option<String> {
    match kind {
        1 => Some(raw.iter().map(|byte| char::from(*byte)).collect()),
        2 => {
            let (units, rest) = raw.as_chunks::<2>();
            if !rest.is_empty() {
                return None;
            }
            let units: Vec<u16> = units.iter().map(|pair| u16::from_le_bytes(*pair)).collect();
            String::from_utf16(&units).ok()
        }
        4 => {
            let (chunks, rest) = raw.as_chunks::<4>();
            if !rest.is_empty() {
                return None;
            }
            let mut out = String::new();
            for chunk in chunks {
                let value = u32::from_le_bytes(*chunk);
                out.push(char::from_u32(value)?);
            }
            Some(out)
        }
        _ => None,
    }
}

fn fixed_312() -> Offsets {
    Offsets {
        interpreters_head: 40,
        interp_next: 0,
        threads_head: 72,
        tstate_next: 8,
        cframe: Some(56),
        current_frame: 0,
        native_thread_id: if cfg!(windows) { 140 } else { 144 },
        frame_code: 0,
        frame_previous: 8,
        frame_instr: 56,
        code_filename: 112,
        code_qualname: 128,
        code_linetable: 136,
        code_firstlineno: 68,
        code_adaptive: 192,
        str_state: 32,
        str_length: 16,
        str_ascii: 40,
        str_compact: 56,
        bytes_size: 16,
        bytes_data: 32,
    }
}

fn fixed_311() -> Offsets {
    Offsets {
        interpreters_head: 40,
        interp_next: 0,
        threads_head: 16,
        tstate_next: 8,
        cframe: Some(56),
        // `_PyCFrame.use_tracing` is a byte, so the pointer is at 8.
        current_frame: 8,
        // `unsigned long` is 4 bytes on Windows, so this field is 4 earlier.
        native_thread_id: if cfg!(windows) { 156 } else { 160 },
        frame_code: 32,
        frame_previous: 48,
        frame_instr: 56,
        code_filename: 112,
        code_qualname: 128,
        code_linetable: 136,
        code_firstlineno: 72,
        code_adaptive: 184,
        str_state: 32,
        str_length: 16,
        // `PyASCIIObject.wstr` puts compact ASCII data at 48. Compact
        // non-ASCII data follows `wstr_length`, at 72.
        str_ascii: 48,
        str_compact: 72,
        bytes_size: 16,
        bytes_data: 32,
    }
}

fn layout_for(path: &Path, image: &PythonImage) -> Result<Offsets, String> {
    let Some((major, minor, micro)) = read_version(path, image) else {
        return Err(
            "CPython was detected from `_PyRuntime`, but `Py_Version` is not exported. The default GIL builds of 3.11, 3.12, and 3.13 export that symbol. This process was not walked"
                .to_string(),
        );
    };
    let label = format!("{major}.{minor}.{micro}");
    if major != 3 || !matches!(minor, 11..=13) {
        return Err(format!(
            "CPython {label} was detected and is not walked. Supported builds are the default GIL layouts of 3.11, 3.12, and 3.13"
        ));
    }
    let cookie = runtime_cookie(path, image);
    if matches!(minor, 11 | 12) {
        if cookie {
            return Err(format!(
                "CPython {label} was detected, but `_PyRuntime` starts with the 3.13 `xdebugpy` cookie. That build is not walked"
            ));
        }
        return Ok(if minor == 11 {
            fixed_311()
        } else {
            fixed_312()
        });
    }
    if !cookie {
        return Err(format!(
            "CPython {label} was detected, but `_PyRuntime` has no `xdebugpy` layout table. This build is not walked"
        ));
    }
    let free = image
        .runtime_file
        .checked_add(16)
        .and_then(|offset| read_file_u64(path, offset))
        .unwrap_or(u64::MAX);
    if free != 0 {
        return Err(format!(
            "CPython {label} free-threaded build was detected and is not walked. Supported builds are the default GIL layouts of 3.11, 3.12, and 3.13"
        ));
    }
    debug_offsets(path, image.runtime_file).ok_or_else(|| {
        format!(
            "CPython {label} was detected, but its `_Py_DebugOffsets` table is not a supported GIL layout. This build was not walked"
        )
    })
}

fn read_version(path: &Path, image: &PythonImage) -> Option<(u32, u32, u32)> {
    let mut version = [0u8; 4];
    read_file(path, image.version_file?, &mut version)?;
    let hex = u32::from_le_bytes(version);
    let major = hex >> 24;
    let minor = (hex >> 16) & 0xff;
    let micro = (hex >> 8) & 0xff;
    (major > 0).then_some((major, minor, micro))
}

fn runtime_cookie(path: &Path, image: &PythonImage) -> bool {
    let mut cookie = [0u8; 8];
    read_file(path, image.runtime_file, &mut cookie).is_some() && &cookie == b"xdebugpy"
}

fn debug_offsets(path: &Path, runtime_file: u64) -> Option<Offsets> {
    let at = |slot: u64| read_file_u64(path, runtime_file.checked_add(slot)?);
    // Sizes are the first field of each group in `_Py_DebugOffsets`.
    // 3.13's interpreter state is far larger than 4 KiB, so the bound is
    // that size rather than a fixed cap. An inserted field in a later
    // minor version makes an offset fall outside its size.
    let runtime_size = at(24)?;
    let interp_size = at(48)?;
    let thread_size = at(152)?;
    let frame_size = at(224)?;
    let code_size = at(272)?;
    let bytes_object = at(512)?;
    let unicode_object = at(536)?;
    if !plausible_size(runtime_size, 64, 1 << 20)
        || !plausible_size(interp_size, 64, 1 << 20)
        || !plausible_size(thread_size, 32, 4096)
        || !plausible_size(frame_size, 16, 1024)
        || !plausible_size(code_size, 32, 4096)
        || !plausible_size(bytes_object, 16, 256)
        || !plausible_size(unicode_object, 16, 256)
    {
        return None;
    }
    let interpreters_head = at(40)?;
    let interp_next = at(64)?;
    let threads_head = at(72)?;
    let tstate_next = at(168)?;
    let current_frame = at(184)?;
    let native_thread_id = at(200)?;
    let frame_previous = at(232)?;
    let frame_code = at(240)?;
    let frame_instr = at(248)?;
    let code_filename = at(280)?;
    let code_qualname = at(296)?;
    let code_linetable = at(304)?;
    let code_firstlineno = at(312)?;
    let code_adaptive = at(344)?;
    let bytes_size = at(520)?;
    let bytes_data = at(528)?;
    let str_state = at(544)?;
    let str_length = at(552)?;
    let str_ascii = at(560)?;
    // 3.13 compact non-ASCII data follows `utf8_length` and `utf8`.
    let str_compact = str_ascii.checked_add(16)?;
    let inside = [
        (interpreters_head, runtime_size),
        (interp_next, interp_size),
        (threads_head, interp_size),
        (tstate_next, thread_size),
        (current_frame, thread_size),
        (native_thread_id, thread_size),
        (frame_previous, frame_size),
        (frame_code, frame_size),
        (frame_instr, frame_size),
        (code_filename, code_size),
        (code_qualname, code_size),
        (code_linetable, code_size),
        (code_firstlineno, code_size),
        (code_adaptive, code_size),
        (bytes_size, bytes_object),
        (bytes_data, bytes_object),
        (str_state, unicode_object),
        (str_length, unicode_object),
    ];
    // `frame_code`, `frame_previous`, and `interp_next` may be zero.
    // The rest are fields past the start of their struct in every 3.13 build.
    let required = [
        interpreters_head,
        threads_head,
        tstate_next,
        current_frame,
        native_thread_id,
        frame_instr,
        code_filename,
        code_qualname,
        code_linetable,
        code_firstlineno,
        code_adaptive,
        bytes_size,
        bytes_data,
        str_state,
        str_length,
    ];
    if inside
        .iter()
        .any(|(offset, size)| *offset >= *size || !offset.is_multiple_of(4))
        || required.contains(&0)
        || !(16..=128).contains(&str_ascii)
        || !str_ascii.is_multiple_of(8)
        || bytes_data <= bytes_size
    {
        return None;
    }
    Some(Offsets {
        interpreters_head,
        interp_next,
        threads_head,
        tstate_next,
        cframe: None,
        current_frame,
        native_thread_id,
        frame_code,
        frame_previous,
        frame_instr,
        code_filename,
        code_qualname,
        code_linetable,
        code_firstlineno,
        code_adaptive,
        str_state,
        str_length,
        str_ascii,
        str_compact,
        bytes_size,
        bytes_data,
    })
}

fn plausible_size(size: u64, min: u64, max: u64) -> bool {
    (min..=max).contains(&size) && size.is_multiple_of(4)
}

fn read_file(path: &Path, offset: u64, buf: &mut [u8]) -> Option<()> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path).ok()?;
    file.seek(SeekFrom::Start(offset)).ok()?;
    file.read_exact(buf).ok()
}

fn read_file_u64(path: &Path, offset: u64) -> Option<u64> {
    let mut buf = [0u8; 8];
    read_file(path, offset, &mut buf)?;
    Some(u64::from_le_bytes(buf))
}

#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
fn open_elf(path: &Path, spans: &[crate::maps::MapSpan]) -> Option<PythonVm> {
    let image = python_sym::exports(path)?;
    let offsets = layout_for(path, &image).ok()?;
    let anchor = spans
        .iter()
        .find(|span| span.executable)
        .or_else(|| spans.first())?;
    let slide = crate::elf::load_bias(
        anchor.start,
        anchor.offset,
        &image.loads,
        crate::elf::system_page_size(),
    )?;
    let start = spans.iter().map(|span| span.start).min()?;
    let end = spans.iter().map(|span| span.end).max()?;
    if end <= start {
        return None;
    }
    Some(PythonVm {
        runtime: image.runtime.wrapping_add(slide),
        image_start: start,
        image_end: end,
        offsets,
    })
}

#[cfg(any(target_os = "macos", windows))]
fn open_slid(path: &Path, slide: u64, start: u64, end: u64) -> Option<PythonVm> {
    if end <= start {
        return None;
    }
    let image = python_sym::exports(path)?;
    let offsets = layout_for(path, &image).ok()?;
    Some(PythonVm {
        runtime: image.runtime.wrapping_add(slide),
        image_start: start,
        image_end: end,
        offsets,
    })
}

fn mentions_python(path: &str) -> bool {
    let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
    let lower = name.to_ascii_lowercase();
    lower.contains("python") || lower.contains("libpython")
}

/// Why a process is or is not a walked CPython.
enum Support {
    Absent,
    Supported,
    Rejected(String),
}

/// Refuses a python-backend profile that would not contain Python frames.
///
/// Detection only looks for `_PyRuntime`. This then requires one of the
/// walked layouts: the default GIL builds of 3.11, 3.12, and 3.13. Anything
/// else, including an older CPython, 3.14, and a free-threaded 3.13, is an
/// error. On Linux `x86_64` and `aarch64` a supported process is also probed
/// with `process_vm_readv`. `EPERM` and `EACCES` are errors that name the errno
/// and `kernel.yama.ptrace_scope`. anyprof does not change that sysctl.
/// Either failure returns before a native interpreter stack can be recorded
/// under the python backend.
pub fn prepare_python(pid: u32) -> Result<(), Error> {
    if pid == 0 {
        return Ok(());
    }
    match cpython_support(pid) {
        Support::Absent => Ok(()),
        Support::Rejected(message) => Err(Error::backend("python", message)),
        Support::Supported => {
            #[cfg(all(
                target_os = "linux",
                any(target_arch = "x86_64", target_arch = "aarch64")
            ))]
            {
                let Some(vm) = attach_linux(pid) else {
                    return Err(Error::backend(
                        "python",
                        "CPython was detected but its load address could not be resolved, so the frame chain was not read",
                    ));
                };
                probe_runtime(pid, vm.runtime)?;
            }
            Ok(())
        }
    }
}

fn cpython_support(pid: u32) -> Support {
    #[cfg(target_os = "linux")]
    {
        classify_paths(&candidate_paths(pid))
    }
    #[cfg(target_os = "macos")]
    {
        classify_paths(&macos_candidates(pid))
    }
    #[cfg(windows)]
    {
        classify_paths(&windows_candidates(pid))
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        let _ = pid;
        Support::Absent
    }
}

fn classify_paths(paths: &[PathBuf]) -> Support {
    let mut rejected = None;
    for path in paths {
        let Some(image) = python_sym::exports(path) else {
            continue;
        };
        match layout_for(path, &image) {
            Ok(_) => return Support::Supported,
            Err(message) => {
                if rejected.is_none() {
                    rejected = Some(message);
                }
            }
        }
    }
    match rejected {
        Some(message) => Support::Rejected(message),
        None => Support::Absent,
    }
}

#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
fn probe_runtime(pid: u32, runtime: u64) -> Result<(), Error> {
    let mut buf = [0u8; 8];
    match read_process_errno(pid, runtime, &mut buf) {
        Ok(()) => Ok(()),
        Err(err) => Err(Error::backend("python", read_denied_message(pid, err))),
    }
}

#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
fn read_denied_message(pid: u32, err: i32) -> String {
    let scope = std::fs::read_to_string("/proc/sys/kernel/yama/ptrace_scope")
        .ok()
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty());
    let scope = match scope {
        Some(value) => format!("kernel.yama.ptrace_scope is {value}"),
        None => "kernel.yama.ptrace_scope could not be read".to_string(),
    };
    let os = std::io::Error::from_raw_os_error(err);
    if err == libc::EPERM || err == libc::EACCES {
        format!(
            "cannot read the frame chain of pid {pid}: process_vm_readv failed with errno {err} ({os}). {scope}. Reading a CPython frame uses ptrace attach permission. perf_event_open permission is not enough. An ancestor of the target, root, or scope 0 can read it. anyprof does not change this sysctl"
        )
    } else {
        format!(
            "cannot read the frame chain of pid {pid}: process_vm_readv of _PyRuntime failed with errno {err} ({os}). {scope}. The profile was not recorded"
        )
    }
}

/// True when the process image exports `_PyRuntime`. This does not attach.
#[must_use]
pub fn pid_is_cpython(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    #[cfg(target_os = "linux")]
    {
        linux_exports(pid)
    }
    #[cfg(target_os = "macos")]
    {
        macos_exports(pid)
    }
    #[cfg(windows)]
    {
        windows_exports(pid)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        let _ = pid;
        false
    }
}

#[cfg(target_os = "linux")]
fn linux_exports(pid: u32) -> bool {
    candidate_paths(pid)
        .iter()
        .any(|path| python_sym::exports(path).is_some())
}

#[cfg(target_os = "linux")]
fn candidate_paths(pid: u32) -> Vec<PathBuf> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/maps")).unwrap_or_default();
    let spans = crate::maps::parse_maps(&text);
    let mut out = Vec::new();
    if let Some(exe) = anyprof_core::executable_file(pid) {
        out.push(exe);
    }
    for span in spans {
        let mapped = crate::maps::live_path(&span.path);
        if !mentions_python(mapped) {
            continue;
        }
        let path = crate::maps::resolve_mapped_path(pid, mapped);
        if !out.contains(&path) {
            out.push(path);
        }
    }
    out
}

/// Opens the interpreter for a Linux process. `None` keeps the native walk.
#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
pub(crate) fn attach_linux(pid: u32) -> Option<PythonVm> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/maps")).ok()?;
    let spans = crate::maps::parse_maps(&text);
    let mut grouped: Vec<(String, Vec<crate::maps::MapSpan>)> = Vec::new();
    for span in spans {
        let mapped = crate::maps::live_path(&span.path).to_string();
        if let Some((_, group)) = grouped.iter_mut().find(|(existing, _)| existing == &mapped) {
            group.push(span);
        } else {
            grouped.push((mapped, vec![span]));
        }
    }
    let exe_target = std::fs::read_link(format!("/proc/{pid}/exe"))
        .ok()
        .map(|link| crate::maps::live_path(&link.to_string_lossy()).to_string());
    let exe_file = anyprof_core::executable_file(pid);
    let mut ordered: Vec<(PathBuf, Vec<crate::maps::MapSpan>)> = Vec::new();
    if let Some(target) = &exe_target
        && let Some(pos) = grouped.iter().position(|(mapped, _)| mapped == target)
    {
        let (mapped, group) = grouped.remove(pos);
        let resolved = crate::maps::resolve_mapped_path(pid, &mapped);
        if resolved != Path::new(&mapped) || resolved.is_file() {
            ordered.push((resolved, group));
        } else if let Some(exe) = exe_file {
            ordered.push((exe, group));
        }
    }
    for (mapped, group) in grouped {
        if mentions_python(&mapped) {
            let resolved = crate::maps::resolve_mapped_path(pid, &mapped);
            ordered.push((resolved, group));
        }
    }
    for (path, spans) in ordered {
        if let Some(vm) = open_elf(&path, &spans) {
            tracing::debug!(path = %path.display(), "cpython frame walk attached");
            return Some(vm);
        }
    }
    None
}

/// Copies `buf` from another process. The kernel checks ptrace attach
/// permission (`PTRACE_MODE_ATTACH_REALCREDS`), not the perf read check.
/// `kernel.yama.ptrace_scope` of 1 therefore allows a parent and refuses an
/// unrelated same-user process. A short or failed read returns false.
#[cfg(target_os = "linux")]
pub(crate) fn read_process(pid: u32, addr: u64, buf: &mut [u8]) -> bool {
    read_process_errno(pid, addr, buf).is_ok()
}

#[cfg(target_os = "linux")]
fn read_process_errno(pid: u32, addr: u64, buf: &mut [u8]) -> Result<(), i32> {
    if buf.is_empty() || addr < 0x1000 {
        return Err(libc::EFAULT);
    }
    let local = libc::iovec {
        iov_base: buf.as_mut_ptr().cast(),
        iov_len: buf.len(),
    };
    let remote = libc::iovec {
        iov_base: addr as usize as *mut libc::c_void,
        iov_len: buf.len(),
    };
    // SAFETY: both iovecs describe this call's buffers. `process_vm_readv`
    // only reads the remote process.
    let copied = unsafe { libc::process_vm_readv(pid as libc::pid_t, &local, 1, &remote, 1, 0) };
    if copied == buf.len() as isize {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error()
            .raw_os_error()
            .unwrap_or(libc::EIO))
    }
}

/// `(path, header address, end address)` from the dyld image list.
#[cfg(target_os = "macos")]
pub(crate) fn attach_mach(images: &[(String, u64, u64)]) -> Option<PythonVm> {
    for (path, load, end) in images {
        if !mentions_python(path) {
            continue;
        }
        let Some(slide) = crate::go_sym::macho_bias(Path::new(path), *load) else {
            continue;
        };
        if let Some(vm) = open_slid(Path::new(path), slide, *load, *end) {
            tracing::debug!(path, "cpython frame walk attached");
            return Some(vm);
        }
    }
    None
}

#[cfg(target_os = "macos")]
fn macos_candidates(pid: u32) -> Vec<PathBuf> {
    let Some(path) = macos_exe(pid) else {
        return Vec::new();
    };
    let mut out = vec![path.clone()];
    for library in python_sym::linked_python_libraries(&path) {
        if !out.contains(&library) {
            out.push(library);
        }
    }
    out
}

#[cfg(target_os = "macos")]
fn macos_exports(pid: u32) -> bool {
    macos_candidates(pid)
        .iter()
        .any(|path| python_sym::exports(path).is_some())
}

#[cfg(target_os = "macos")]
fn macos_exe(pid: u32) -> Option<PathBuf> {
    const MAX: u32 = 4 * 1024;
    let mut buf = vec![0u8; MAX as usize];
    let pid = i32::try_from(pid).ok()?;
    let n = unsafe { proc_pidpath(pid, buf.as_mut_ptr().cast(), MAX) };
    if n <= 0 {
        return None;
    }
    let end = buf.iter().position(|byte| *byte == 0).unwrap_or(n as usize);
    let text = String::from_utf8(buf[..end].to_vec()).ok()?;
    (!text.is_empty()).then_some(PathBuf::from(text))
}

#[cfg(target_os = "macos")]
#[link(name = "proc")]
unsafe extern "C" {
    fn proc_pidpath(pid: i32, buffer: *mut std::ffi::c_char, buffersize: u32) -> i32;
}

#[cfg(windows)]
pub(crate) fn attach_process(process: windows_sys::Win32::Foundation::HANDLE) -> Option<PythonVm> {
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::System::ProcessStatus::{
        EnumProcessModules, GetModuleFileNameExW, GetModuleInformation, MODULEINFO,
    };

    let mut modules = vec![std::ptr::null_mut(); 1024];
    let mut needed = 0u32;
    let ok = unsafe {
        EnumProcessModules(
            process,
            modules.as_mut_ptr(),
            (modules.len() * std::mem::size_of::<*mut std::ffi::c_void>()) as u32,
            &mut needed,
        )
    };
    if ok == 0 {
        return None;
    }
    let count = (needed as usize / std::mem::size_of::<*mut std::ffi::c_void>()).min(modules.len());
    for module in modules.into_iter().take(count) {
        if module.is_null() {
            continue;
        }
        let mut name = vec![0u16; 32 * 1024];
        let n =
            unsafe { GetModuleFileNameExW(process, module, name.as_mut_ptr(), name.len() as u32) };
        if n == 0 {
            continue;
        }
        let path = PathBuf::from(std::ffi::OsString::from_wide(&name[..n as usize]));
        if !mentions_python(&path.to_string_lossy()) {
            continue;
        }
        let mut info = MODULEINFO {
            lpBaseOfDll: std::ptr::null_mut(),
            SizeOfImage: 0,
            EntryPoint: std::ptr::null_mut(),
        };
        let ok = unsafe {
            GetModuleInformation(
                process,
                module,
                &mut info,
                std::mem::size_of::<MODULEINFO>() as u32,
            )
        };
        if ok == 0 || info.SizeOfImage == 0 {
            continue;
        }
        let base = info.lpBaseOfDll as u64;
        let end = base.saturating_add(u64::from(info.SizeOfImage));
        let Some(image) = python_sym::exports(&path) else {
            continue;
        };
        if image.image_base == 0 {
            continue;
        }
        let image_base = image.image_base;
        let slide = base.wrapping_sub(image_base);
        if let Some(vm) = open_slid(&path, slide, base, end) {
            tracing::debug!(path = %path.display(), "cpython frame walk attached");
            return Some(vm);
        }
    }
    None
}

#[cfg(windows)]
fn windows_candidates(pid: u32) -> Vec<PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
    };

    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        return Vec::new();
    }
    let mut buf = vec![0u16; 32 * 1024];
    let mut len = buf.len() as u32;
    let ok = unsafe { QueryFullProcessImageNameW(handle, 0, buf.as_mut_ptr(), &mut len) };
    unsafe { CloseHandle(handle) };
    if ok == 0 || len == 0 {
        return Vec::new();
    }
    let path = PathBuf::from(std::ffi::OsString::from_wide(&buf[..len as usize]));
    let mut out = vec![path.clone()];
    let Some(dir) = path.parent() else {
        return out;
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let text = entry.file_name();
        let text = text.to_string_lossy();
        if !mentions_python(&text) {
            continue;
        }
        let path = entry.path();
        if !out.contains(&path) {
            out.push(path);
        }
    }
    out
}

#[cfg(windows)]
fn windows_exports(pid: u32) -> bool {
    windows_candidates(pid)
        .iter()
        .any(|path| python_sym::exports(path).is_some())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_line_table_entry_keeps_the_first_line() {
        // ONE_LINE0, one code unit, two column bytes. Byte offset 0 is line 10.
        let table = [208u8, 0, 0];
        assert_eq!(addr2line(&table, 10, 0), Some(10));
        assert_eq!(addr2line(&table, 10, -1), Some(10));
    }

    #[test]
    fn line_delta_reads_a_signed_varint() {
        // NO_COLUMNS (13) with a signed varint of +2 (value 4, low bit clear).
        let entry = [128 | (13 << 3), 4];
        assert_eq!(line_delta(&entry), 2);
    }

    #[test]
    fn debug_offsets_accept_a_313_interpreter() {
        // Sizes and offsets copied from CPython 3.13.18's `xdebugpy` table.
        // The interpreter state is larger than 4 KiB, and the frame's code
        // pointer sits at offset 0.
        let mut bytes = vec![0u8; 568];
        bytes[..8].copy_from_slice(b"xdebugpy");
        let put = |buf: &mut [u8], slot: usize, value: u64| {
            buf[slot..slot + 8].copy_from_slice(&value.to_le_bytes());
        };
        put(&mut bytes, 24, 283_408);
        put(&mut bytes, 40, 632);
        put(&mut bytes, 48, 194_984);
        put(&mut bytes, 64, 7_264);
        put(&mut bytes, 72, 7_344);
        put(&mut bytes, 152, 312);
        put(&mut bytes, 168, 8);
        put(&mut bytes, 184, 72);
        put(&mut bytes, 200, 160);
        put(&mut bytes, 224, 80);
        put(&mut bytes, 232, 8);
        put(&mut bytes, 240, 0);
        put(&mut bytes, 248, 56);
        put(&mut bytes, 272, 208);
        put(&mut bytes, 280, 112);
        put(&mut bytes, 296, 128);
        put(&mut bytes, 304, 136);
        put(&mut bytes, 312, 68);
        put(&mut bytes, 344, 200);
        put(&mut bytes, 512, 40);
        put(&mut bytes, 520, 16);
        put(&mut bytes, 528, 32);
        put(&mut bytes, 536, 64);
        put(&mut bytes, 544, 32);
        put(&mut bytes, 552, 16);
        put(&mut bytes, 560, 40);
        // 3.13.18 final. The version sits past the debug table.
        let version_at = bytes.len();
        bytes.extend_from_slice(&0x030D_12F0u32.to_le_bytes());
        let path =
            std::env::temp_dir().join(format!("anyprof-py313-offsets-{}", std::process::id()));
        std::fs::write(&path, &bytes).unwrap();
        let image = PythonImage {
            runtime: 0,
            runtime_file: 0,
            version: 0,
            version_file: Some(version_at as u64),
            eval: 0,
            eval_size: 0,
            loads: Vec::new(),
            text_vmaddr: 0,
            image_base: 0,
        };
        let offsets = layout_for(&path, &image).expect("3.13 layout");
        assert_eq!(offsets.interpreters_head, 632);
        assert_eq!(offsets.interp_next, 7_264);
        assert_eq!(offsets.frame_code, 0);
        assert_eq!(offsets.str_ascii, 40);
        assert_eq!(offsets.str_compact, 56);
        assert!(offsets.cframe.is_none());
        bytes[40] = 0xff;
        std::fs::write(&path, &bytes).unwrap();
        let err = layout_for(&path, &image).expect_err("a corrupt table is not walked");
        assert!(err.contains("not a supported GIL layout"), "{err}");

        // The same table with a 3.14 version must be rejected even though
        // every 3.13 slot is still in range. The version is the gate.
        let mut later = bytes.clone();
        later[40] = 0x78;
        let version_at = 568;
        later[version_at..version_at + 4].copy_from_slice(&0x030E_00F0u32.to_le_bytes());
        std::fs::write(&path, &later).unwrap();
        let err = layout_for(&path, &image).expect_err("3.14 is not walked");
        assert!(err.contains("3.14.0"), "{err}");
        assert!(err.contains("not walked"), "{err}");

        // A free-threaded 3.13 keeps the cookie and the version. The flag
        // at offset 16 is what rejects it.
        let mut threaded = bytes.clone();
        threaded[40] = 0x78;
        threaded[version_at..version_at + 4].copy_from_slice(&0x030D_12F0u32.to_le_bytes());
        threaded[16..24].copy_from_slice(&1u64.to_le_bytes());
        std::fs::write(&path, &threaded).unwrap();
        let err = layout_for(&path, &image).expect_err("free-threaded is not walked");
        assert!(err.contains("free-threaded"), "{err}");
        assert!(err.contains("not walked"), "{err}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn fixed_layouts_match_the_header_offsets() {
        let dir = std::env::temp_dir().join(format!("anyprof-py-layout-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("image");
        // Version word, then eight bytes that are not the 3.13 cookie.
        let mut bytes = 0x030B_12F0u32.to_le_bytes().to_vec();
        bytes.extend_from_slice(&[0u8; 8]);
        std::fs::write(&path, &bytes).unwrap();
        let mut image = PythonImage {
            runtime: 0,
            runtime_file: 4,
            version: 0,
            version_file: Some(0),
            eval: 0,
            eval_size: 0,
            loads: Vec::new(),
            text_vmaddr: 0,
            image_base: 0,
        };
        let offsets = layout_for(&path, &image).expect("3.11");
        assert_eq!(offsets.current_frame, 8);
        assert_eq!(offsets.cframe, Some(56));
        assert_eq!(offsets.str_ascii, 48);
        assert_eq!(offsets.str_compact, 72);
        assert_eq!(offsets.frame_code, 32);
        bytes[..4].copy_from_slice(&0x030C_10F0u32.to_le_bytes());
        std::fs::write(&path, &bytes).unwrap();
        let offsets = layout_for(&path, &image).expect("3.12");
        assert_eq!(offsets.current_frame, 0);
        assert_eq!(offsets.str_ascii, 40);
        assert_eq!(offsets.str_compact, 56);
        image.version_file = None;
        let err = layout_for(&path, &image).expect_err("no Py_Version");
        assert!(err.contains("Py_Version"), "{err}");
        assert!(err.contains("not walked"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    #[test]
    fn a_cross_user_read_names_process_vm_readv() {
        // pid 1 belongs to another user. The credential check fails before a
        // frame can be invented. Yama may be absent in this kernel; the
        // cross-user denial is the same errno the scope-1 same-user denial uses.
        let mut buf = [0u8; 8];
        let err = read_process_errno(1, 0x10000, &mut buf).expect_err("pid 1 is not readable");
        assert!(
            err == libc::EPERM || err == libc::EACCES,
            "expected a permission errno, got {err}"
        );
        let message = read_denied_message(1, err);
        assert!(message.contains("process_vm_readv"), "{message}");
        assert!(message.contains("ptrace_scope"), "{message}");
        assert!(message.contains("perf_event_open"), "{message}");
        assert!(message.contains("pid 1"), "{message}");
        assert!(
            message.contains("anyprof does not change this sysctl"),
            "{message}"
        );
    }

    #[test]
    fn this_process_is_not_cpython() {
        assert!(!pid_is_cpython(std::process::id()));
        assert!(!pid_is_cpython(0));
    }

    #[cfg(all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    #[test]
    fn reads_a_live_312_frame() {
        let python = Path::new("/usr/bin/python3.12");
        if !python.is_file() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("anyprof-py-unit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("spin.py");
        std::fs::write(
            &script,
            "def caller():\n    spin()\ndef spin():\n    x = 0\n    while True:\n        x = x + 1\ncaller()\n",
        )
        .unwrap();
        let mut child = std::process::Command::new(python)
            .arg(&script)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn python");
        let pid = child.id();
        std::thread::sleep(std::time::Duration::from_millis(200));
        let vm = attach_linux(pid);
        let mut saw = false;
        let started = std::time::Instant::now();
        while started.elapsed() < std::time::Duration::from_secs(2) {
            let Some(vm) = vm.as_ref() else {
                break;
            };
            let Ok(tasks) = std::fs::read_dir(format!("/proc/{pid}/task")) else {
                break;
            };
            for task in tasks.flatten() {
                let Ok(tid) = task.file_name().to_string_lossy().parse::<u64>() else {
                    continue;
                };
                let Some(frames) = vm.frames(tid, vm.image_start, &mut |addr, buf| {
                    read_process(pid, addr, buf)
                }) else {
                    continue;
                };
                if frames.iter().any(|frame| frame.name == "spin")
                    && frames.iter().any(|frame| frame.name == "caller")
                {
                    saw = true;
                    break;
                }
            }
            if saw {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let _ = child.kill();
        let _ = child.wait();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(vm.is_some(), "python 3.12 should open a frame walk");
        assert!(saw, "expected spin and caller on the live frame chain");
    }
}
