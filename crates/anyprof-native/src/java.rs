//! HotSpot Java methods on the native on-CPU sampler.
//!
//! A Java method runs inside the JVM, so a frame-pointer or `StackWalk64`
//! walk names `libjvm`, not `Class.method`. This module reads the same
//! serviceability table the Serviceability Agent uses (`gHotSpotVMStructs`,
//! `gHotSpotVMTypes`, `gHotSpotVMIntConstants`). It does not attach, load an
//! agent, or signal the process. Reads are `process_vm_readv`, Mach VM read,
//! or `ReadProcessMemory`.
//!
//! The leaf program counter has to fall in a code-cache blob.
//!
//! * A blob with a positive `CodeBlob._frame_size` and a `Method*` that
//!   resolves is compiled Java. The caller is `sp + frame_size` words, which
//!   is HotSpot's compiled sender on x86_64 and aarch64. The return address
//!   is the word below that, and the saved frame pointer is two words below
//!   it. Linux and Windows leave `PreserveFramePointer` off, so the frame
//!   pointer is not the walk.
//! * A blob whose name is `Interpreter` and whose frame size is 0 is the
//!   template interpreter. The method is the slot at
//!   `interpreter_frame_last_sp_offset - 1`. That is `-3` words from the
//!   frame pointer when the constants are `sender_sp = -1` and
//!   `last_sp = -2`. x86_64 exports that pair and any other exported pair is
//!   refused. aarch64's `VM_INT_CONSTANTS_CPU` is empty in JDK 17, 21, and 25,
//!   and those three `frame_aarch64.hpp` headers define the same pair. The
//!   empty table is used only when the mapped libjvm is aarch64 (ELF
//!   `e_machine`, the mapped Mach-O `cputype`, or the PE machine) and
//!   `Abstract_VM_Version._vm_major_version` is 17, 21, or 25. A missing pair
//!   on x86_64, on another version, or when only one constant is present, is
//!   refused. The stored sender SP and the saved frame pointer are the caller.
//!
//! A stub, an unreadable method, or a leaf outside the code cache keeps the
//! native walk and is not labeled Java. A denied read does not: `prepare_java`
//! fails `start` before a profile exists. anyprof does not change sysctls.
//!
//! `HeapBlock` and `GrowableArrayBase` come from the same tables. JDK 17 and
//! 21 have a 16-byte `HeapBlock` with `_used` at 8 (`size_t` length) and store
//! the instruction range in `CodeBlob._code_begin` and `_code_end`. JDK 25
//! has an 8-byte `HeapBlock` with `_used` at 4 (`uint32_t` length) and stores
//! `_code_offset` and `_data_offset`; the range is `this + offset`. A JDK 25
//! build that also exports the pointer fields is refused. The code
//! cache array is the 8-byte product `GrowableArrayBase` (`_max` on 17,
//! `_capacity` on 21 and 25, data pointer at 8). The compiled `Method*` is
//! `CompiledMethod._method` on 17 and 21 and `nmethod._method` on 25. A debug
//! JVM, a 32-bit JVM, another version, or a HotSpot whose exported offsets do
//! not match that version is refused instead of walked with a guessed layout.

use std::collections::HashMap;
use std::path::Path;
#[cfg(any(target_os = "linux", windows))]
use std::path::PathBuf;

use anyprof_core::{Error, Frame, Runtime};

const MAX_FRAMES: usize = 48;
const MAX_HEAPS: i32 = 16;
const MAX_STRUCTS: usize = 20_000;
const MAX_TYPES: usize = 8_000;
const MAX_CONSTANTS: usize = 20_000;
const MAX_SEGMAP_HOPS: usize = 1024;
const MAX_SEGMENTS: u64 = 16_000_000;
const MAX_FRAME_WORDS: i32 = 4096;
const MAX_NAME: usize = 200;
const WORD: u64 = 8;

const HOTSPOT_SYMBOLS: &[&str] = &[
    "gHotSpotVMStructs",
    "gHotSpotVMStructEntryTypeNameOffset",
    "gHotSpotVMStructEntryFieldNameOffset",
    "gHotSpotVMStructEntryIsStaticOffset",
    "gHotSpotVMStructEntryOffsetOffset",
    "gHotSpotVMStructEntryAddressOffset",
    "gHotSpotVMStructEntryArrayStride",
    "gHotSpotVMTypes",
    "gHotSpotVMTypeEntryTypeNameOffset",
    "gHotSpotVMTypeEntrySizeOffset",
    "gHotSpotVMTypeEntryArrayStride",
    "gHotSpotVMIntConstants",
    "gHotSpotVMIntConstantEntryNameOffset",
    "gHotSpotVMIntConstantEntryValueOffset",
    "gHotSpotVMIntConstantEntryArrayStride",
];

/// Byte offsets and the interpreter slot this process's HotSpot actually uses.
#[derive(Debug)]
struct Layout {
    code_range: CodeRange,
    frame_size: u64,
    blob_size: u64,
    blob_name: u64,
    compiled_method: u64,
    const_method: u64,
    constants: u64,
    name_index: u64,
    pool_holder: u64,
    klass_name: u64,
    sym_length: u64,
    sym_body: u64,
    heap_memory: u64,
    heap_segmap: u64,
    heap_log2: u64,
    vs_low: u64,
    vs_high: u64,
    heap_block: u64,
    /// Byte offset of `HeapBlock::Header._used` from the start of the block.
    heap_used: u64,
    /// `GrowableArrayBase._len`, the capacity field, and `GrowableArray<int>._data`.
    array_len: u64,
    array_cap: u64,
    array_data: u64,
    cp_size: u64,
    /// Words from the interpreter frame pointer to the `Method*`.
    method_words: i32,
}

/// One HotSpot process whose serviceability table matched the walked layout.
#[derive(Debug)]
pub(crate) struct JavaVm {
    layout: Layout,
    /// Address of the `CodeCache._heaps` pointer.
    heaps: u64,
    low_bound: u64,
    high_bound: u64,
}

impl JavaVm {
    /// Java methods for a sample whose leaf is `pc`.
    ///
    /// `None` means the leaf is not a Java method (outside the code cache, a
    /// stub, or a method pointer that does not resolve). The caller keeps the
    /// native walk. This does not invent a name.
    pub(crate) fn frames<R>(&self, pc: u64, sp: u64, fp: u64, read: &mut R) -> Option<Vec<String>>
    where
        R: FnMut(u64, &mut [u8]) -> bool,
    {
        let mut pc = canonical_user(pc)?;
        let mut sp = canonical_user(sp).unwrap_or(0);
        let mut fp = canonical_user(fp).unwrap_or(0);
        let mut out = Vec::new();
        let mut seen = Vec::new();
        while out.len() < MAX_FRAMES {
            if seen.contains(&(pc, sp)) {
                break;
            }
            seen.push((pc, sp));
            let Some(blob) = self.find_blob(pc, read) else {
                break;
            };
            if blob.interpreter {
                let Some(slot) = word_addr(fp, self.layout.method_words) else {
                    break;
                };
                let Some(method) = read_u64(read, slot) else {
                    break;
                };
                let Some(name) = self.method_name(method, read) else {
                    break;
                };
                out.push(name);
                let Some(next) = interpreter_sender(sp, fp, read) else {
                    break;
                };
                pc = next.0;
                sp = next.1;
                fp = next.2;
                continue;
            }
            if blob.frame_words > 0 {
                let Some(name) = self.method_name(blob.method, read) else {
                    break;
                };
                out.push(name);
                let Some(next) = compiled_sender(sp, blob.frame_words, read) else {
                    break;
                };
                pc = next.0;
                sp = next.1;
                fp = next.2;
                continue;
            }
            break;
        }
        (!out.is_empty()).then_some(out)
    }

    fn find_blob<R>(&self, pc: u64, read: &mut R) -> Option<Blob>
    where
        R: FnMut(u64, &mut [u8]) -> bool,
    {
        let low_bound = read_u64(read, self.low_bound)?;
        let high_bound = read_u64(read, self.high_bound)?;
        if pc < low_bound || pc >= high_bound {
            return None;
        }
        let heaps = read_u64(read, self.heaps)?;
        let len = read_i32(read, heaps.checked_add(self.layout.array_len)?)?;
        let capacity = read_i32(read, heaps.checked_add(self.layout.array_cap)?)?;
        if len <= 0 || len > MAX_HEAPS || capacity < len {
            return None;
        }
        let data = read_u64(read, heaps.checked_add(self.layout.array_data)?)?;
        for index in 0..len {
            let slot = data.checked_add(u64::from(index as u32) * WORD)?;
            let heap = read_u64(read, slot)?;
            if let Some(blob) = self.blob_in_heap(heap, pc, read) {
                return Some(blob);
            }
        }
        None
    }

    fn blob_in_heap<R>(&self, heap: u64, pc: u64, read: &mut R) -> Option<Blob>
    where
        R: FnMut(u64, &mut [u8]) -> bool,
    {
        let low = read_u64(
            read,
            heap.checked_add(self.layout.heap_memory + self.layout.vs_low)?,
        )?;
        let high = read_u64(
            read,
            heap.checked_add(self.layout.heap_memory + self.layout.vs_high)?,
        )?;
        if pc < low || pc >= high {
            return None;
        }
        let log2 = read_i32(read, heap.checked_add(self.layout.heap_log2)?)?;
        if !(1..16).contains(&log2) {
            return None;
        }
        let segmap = read_u64(
            read,
            heap.checked_add(self.layout.heap_segmap + self.layout.vs_low)?,
        )?;
        let mut seg = pc.checked_sub(low)? >> log2;
        if seg > MAX_SEGMENTS {
            return None;
        }
        for _ in 0..MAX_SEGMAP_HOPS {
            let mut byte = [0u8; 1];
            if !read(segmap.checked_add(seg)?, &mut byte) {
                return None;
            }
            if byte[0] == 0xff {
                return None;
            }
            if byte[0] == 0 {
                break;
            }
            seg = seg.checked_sub(u64::from(byte[0]))?;
        }
        let block = low.checked_add(seg << log2)?;
        let mut used = [0u8; 1];
        if !read(block.checked_add(self.layout.heap_used)?, &mut used) || used[0] == 0 {
            return None;
        }
        let blob = block.checked_add(self.layout.heap_block)?;
        let frame_words = read_i32(read, blob.checked_add(self.layout.frame_size)?)?;
        let size = read_i32(read, blob.checked_add(self.layout.blob_size)?)?;
        if size <= 0 {
            return None;
        }
        let (begin, end) = code_bounds(blob, size, self.layout.code_range, read)?;
        if pc < begin || pc >= end {
            return None;
        }
        let name = read_u64(read, blob.checked_add(self.layout.blob_name)?).unwrap_or(0);
        let interpreter = read_cstr(read, name, 64).is_some_and(|text| text == "Interpreter");
        let method = if !interpreter
            && frame_words > 0
            && u64::try_from(size).ok()? >= self.layout.compiled_method.checked_add(WORD)?
        {
            read_u64(read, blob.checked_add(self.layout.compiled_method)?).unwrap_or(0)
        } else {
            0
        };
        Some(Blob {
            frame_words,
            interpreter,
            method,
        })
    }

    fn method_name<R>(&self, method: u64, read: &mut R) -> Option<String>
    where
        R: FnMut(u64, &mut [u8]) -> bool,
    {
        if method < 0x1_0000 || !method.is_multiple_of(WORD) {
            return None;
        }
        let const_method = read_u64(read, method.checked_add(self.layout.const_method)?)?;
        if const_method < 0x1_0000 || !const_method.is_multiple_of(WORD) {
            return None;
        }
        let pool = read_u64(read, const_method.checked_add(self.layout.constants)?)?;
        if pool < 0x1_0000 || !pool.is_multiple_of(WORD) {
            return None;
        }
        let name_index = read_u16(read, const_method.checked_add(self.layout.name_index)?)?;
        let holder = read_u64(read, pool.checked_add(self.layout.pool_holder)?)?;
        let class_symbol = read_u64(read, holder.checked_add(self.layout.klass_name)?)?;
        let class_name = symbol(read, class_symbol, &self.layout)?;
        let slot = pool
            .checked_add(self.layout.cp_size)?
            .checked_add(u64::from(name_index) * WORD)?;
        let method_symbol = read_u64(read, slot)?;
        let method_name = symbol(read, method_symbol, &self.layout)?;
        if class_name.contains(';') || method_name.contains(';') {
            return None;
        }
        Some(format!("{}.{}", class_name.replace('/', "."), method_name))
    }
}

struct Blob {
    frame_words: i32,
    interpreter: bool,
    method: u64,
}

/// `Class.method` frames. Kernel addresses stay with the caller.
pub(crate) fn java_profile_frames(names: &[String]) -> Vec<Frame> {
    names
        .iter()
        .map(|name| Frame::new(Runtime::Java, name.clone()))
        .collect()
}

/// Refuses a java-backend profile that would not contain Java methods.
///
/// Detection only looks for `libjvm`. This then requires the serviceability
/// table and the interpreter constants described above. On Linux x86_64 the
/// table is read with `process_vm_readv`. `EPERM` and `EACCES` name the errno
/// and `kernel.yama.ptrace_scope`. anyprof does not change that sysctl.
pub fn prepare_java(pid: u32) -> Result<(), Error> {
    if !pid_is_hotspot(pid) {
        return Err(Error::backend(
            "java",
            format!("pid {pid} is not a HotSpot process (libjvm was not found)"),
        ));
    }
    open_vm(pid).map(|_| ()).map_err(as_java)
}

/// True when the process has loaded HotSpot's `libjvm`. This does not attach
/// and does not read the serviceability table.
#[must_use]
pub fn pid_is_hotspot(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    #[cfg(target_os = "linux")]
    {
        linux_has_libjvm(pid)
    }
    #[cfg(target_os = "macos")]
    {
        macos_has_libjvm(pid)
    }
    #[cfg(windows)]
    {
        windows_has_libjvm(pid)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        let _ = pid;
        false
    }
}

fn open_vm(pid: u32) -> Result<JavaVm, Error> {
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        open_linux(pid).map_err(|message| Error::backend("java", message))
    }
    #[cfg(target_os = "macos")]
    {
        crate::macos::with_task(pid, |task, images| {
            let mut read = |addr: u64, buf: &mut [u8]| crate::macos::read_memory(task, addr, buf);
            open_mach(images, &mut read)
        })
        .map_err(as_java)
    }
    #[cfg(all(windows, target_arch = "x86_64"))]
    {
        crate::windows::with_process(pid, |process| {
            let mut read = |addr: u64, buf: &mut [u8]| read_windows(process, addr, buf);
            open_windows(process, &mut read)
        })
        .map_err(as_java)
    }
    #[cfg(not(any(
        all(target_os = "linux", target_arch = "x86_64"),
        target_os = "macos",
        all(windows, target_arch = "x86_64"),
    )))]
    {
        let _ = pid;
        Err(Error::backend(
            "java",
            "HotSpot frames are not read on this platform",
        ))
    }
}

fn as_java(err: Error) -> Error {
    match err {
        Error::Backend { message, .. } => Error::backend("java", message),
        Error::NotImplemented { detail, .. } => Error::not_implemented("java", detail),
        other => Error::backend("java", other.to_string()),
    }
}

/// Opens the JVM for a Linux x86_64 process. `None` keeps the native walk.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub(crate) fn attach_linux(pid: u32) -> Option<JavaVm> {
    match open_linux(pid) {
        Ok(vm) => Some(vm),
        Err(message) => {
            tracing::debug!(pid, message, "hotspot frame walk was not attached");
            None
        }
    }
}

/// Opens the JVM from a dyld image list. `read` copies remote bytes.
#[cfg(target_os = "macos")]
pub(crate) fn attach_mach<R>(images: &[(String, u64, u64)], read: &mut R) -> Option<JavaVm>
where
    R: FnMut(u64, &mut [u8]) -> bool,
{
    match open_mach(images, read) {
        Ok(vm) => Some(vm),
        Err(message) => {
            tracing::debug!(message, "hotspot frame walk was not attached");
            None
        }
    }
}

/// Opens the JVM from an already-open process. `read` copies remote bytes.
#[cfg(all(windows, target_arch = "x86_64"))]
pub(crate) fn attach_process<R>(
    process: windows_sys::Win32::Foundation::HANDLE,
    read: &mut R,
) -> Option<JavaVm>
where
    R: FnMut(u64, &mut [u8]) -> bool,
{
    match open_windows(process, read) {
        Ok(vm) => Some(vm),
        Err(message) => {
            tracing::debug!(message, "hotspot frame walk was not attached");
            None
        }
    }
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn open_linux(pid: u32) -> Result<JavaVm, String> {
    let (path, spans) = linux_libjvm(pid)?;
    let file = elf_hotspot(&path)?;
    let arch = file.arch;
    let slide = elf_slide(&spans, &path, file.load_offset, file.load_vaddr).ok_or_else(|| {
        format!(
            "libjvm at {} is mapped, but its load bias could not be resolved",
            path.display()
        )
    })?;
    let mut runtime = HashMap::new();
    for (name, value) in file.values {
        runtime.insert(name, slide.wrapping_add(value));
    }
    let probe = *runtime
        .get("gHotSpotVMStructs")
        .ok_or_else(|| "libjvm does not export gHotSpotVMStructs".to_string())?;
    probe_linux(pid, probe)?;
    load_layout(
        &runtime,
        &mut |addr, buf| linux_read(pid, addr, buf).is_ok(),
        arch,
    )
}

#[cfg(target_os = "macos")]
fn open_mach<R>(images: &[(String, u64, u64)], read: &mut R) -> Result<JavaVm, String>
where
    R: FnMut(u64, &mut [u8]) -> bool,
{
    let (path, load, _) = images
        .iter()
        .find(|(path, _, _)| is_libjvm(path))
        .ok_or_else(|| "libjvm was not in the process image list".to_string())?;
    let arch = mapped_mach_arch(*load, read)?;
    let symbols = macho_hotspot(Path::new(path), *load, arch)?;
    load_layout(&symbols, read, arch)
}

#[cfg(all(windows, target_arch = "x86_64"))]
fn open_windows<R>(
    process: windows_sys::Win32::Foundation::HANDLE,
    read: &mut R,
) -> Result<JavaVm, String>
where
    R: FnMut(u64, &mut [u8]) -> bool,
{
    let (path, base) = windows_libjvm(process)?;
    let (symbols, arch) = pe_hotspot(&path, base)?;
    load_layout(&symbols, read, arch)
}

fn load_layout<R>(
    symbols: &HashMap<String, u64>,
    read: &mut R,
    arch: JvmArch,
) -> Result<JavaVm, String>
where
    R: FnMut(u64, &mut [u8]) -> bool,
{
    let mut resolved = HashMap::new();
    for name in HOTSPOT_SYMBOLS {
        let addr = symbols.get(*name).copied().ok_or_else(|| {
            format!("libjvm does not export {name}, so Java methods cannot be read")
        })?;
        let value = read_u64(read, addr).ok_or_else(|| {
            format!("cannot read HotSpot symbol {name}, so Java methods were not walked")
        })?;
        // Offsets are byte displacements inside a serviceability entry.
        // `typeName` is the first field, so that offset is 0. A pointer or a
        // stride of 0 is a bad read, not a layout.
        if value == 0 && !name.ends_with("Offset") {
            return Err(format!(
                "HotSpot symbol {name} is zero, so Java methods were not walked"
            ));
        }
        resolved.insert(*name, value);
    }
    let sym = |name: &str| -> u64 { resolved[name] };
    let stride = sym("gHotSpotVMStructEntryArrayStride");
    let type_name = sym("gHotSpotVMStructEntryTypeNameOffset");
    let field_name = sym("gHotSpotVMStructEntryFieldNameOffset");
    let is_static = sym("gHotSpotVMStructEntryIsStaticOffset");
    let field_offset = sym("gHotSpotVMStructEntryOffsetOffset");
    let field_address = sym("gHotSpotVMStructEntryAddressOffset");
    let structs = sym("gHotSpotVMStructs");
    if !(16..256).contains(&stride) {
        return Err(format!(
            "HotSpot struct entry stride is {stride}, which this build does not walk"
        ));
    }
    let mut fields: HashMap<(String, String), (bool, u64, u64)> = HashMap::new();
    for index in 0..MAX_STRUCTS {
        let entry = structs
            .checked_add(index as u64 * stride)
            .ok_or_else(|| "HotSpot struct table is too large to walk".to_string())?;
        let type_ptr = read_u64(read, entry.checked_add(type_name).unwrap_or(0)).unwrap_or(0);
        if type_ptr == 0 {
            break;
        }
        let Some(ty) = read_cstr(read, type_ptr, 128) else {
            continue;
        };
        let name_ptr = read_u64(read, entry.checked_add(field_name).unwrap_or(0)).unwrap_or(0);
        let Some(name) = read_cstr(read, name_ptr, 128) else {
            continue;
        };
        let static_flag = read_i32(read, entry.checked_add(is_static).unwrap_or(0)).unwrap_or(0);
        let offset = read_u64(read, entry.checked_add(field_offset).unwrap_or(0)).unwrap_or(0);
        let address = if static_flag != 0 {
            read_u64(read, entry.checked_add(field_address).unwrap_or(0)).unwrap_or(0)
        } else {
            0
        };
        fields.insert((ty, name), (static_flag != 0, offset, address));
    }
    let instance = |ty: &str, name: &str| -> Result<u64, String> {
        let (is_static, offset, _) = fields.get(&(ty.to_string(), name.to_string())).ok_or_else(
            || {
                format!(
                    "HotSpot VM structs are missing {ty}.{name}, so Java methods were not walked"
                )
            },
        )?;
        if *is_static || *offset > 1_000_000 {
            return Err(format!(
                "HotSpot field {ty}.{name} is not a usable instance offset"
            ));
        }
        Ok(*offset)
    };
    let static_addr = |ty: &str, name: &str| -> Result<u64, String> {
        let (is_static, _, address) = fields.get(&(ty.to_string(), name.to_string())).ok_or_else(
            || {
                format!(
                    "HotSpot VM structs are missing {ty}.{name}, so Java methods were not walked"
                )
            },
        )?;
        if !*is_static || *address < 0x1_0000 {
            return Err(format!(
                "HotSpot field {ty}.{name} is not a usable static address"
            ));
        }
        Ok(*address)
    };
    let types = read_type_sizes(read, &resolved)?;
    let major = hotspot_major(
        read,
        static_addr("Abstract_VM_Version", "_vm_major_version")?,
    )?;
    let heap_block = *types.get("HeapBlock").ok_or_else(|| {
        "HotSpot VM types are missing HeapBlock, so Java methods were not walked".to_string()
    })?;
    let heap_used = heap_used_offset(
        major,
        heap_block,
        instance("HeapBlock", "_header")?,
        instance("HeapBlock::Header", "_length")?,
        instance("HeapBlock::Header", "_used")?,
    )?;
    let array_base = *types.get("GrowableArrayBase").ok_or_else(|| {
        "HotSpot VM types are missing GrowableArrayBase, so the code cache was not walked"
            .to_string()
    })?;
    let cap_field = match major {
        17 => "_max",
        21 | 25 => "_capacity",
        _ => {
            return Err(format!(
                "HotSpot {major} is not a walked LTS (17, 21, or 25), so Java methods were not walked"
            ));
        }
    };
    let (array_len, array_cap, array_data) = array_offsets(
        major,
        array_base,
        instance("GrowableArrayBase", "_len")?,
        instance("GrowableArrayBase", cap_field)?,
        instance("GrowableArray<int>", "_data")?,
    )?;
    let cp_size = *types.get("ConstantPool").ok_or_else(|| {
        "HotSpot VM types are missing ConstantPool, so method names were not read".to_string()
    })?;
    if !(8..4096).contains(&cp_size) || !cp_size.is_multiple_of(WORD) {
        return Err(format!(
            "ConstantPool is {cp_size} bytes, which this build does not use for method names"
        ));
    }
    let vs_low = instance("VirtualSpace", "_low")?;
    if vs_low == 0 || !vs_low.is_multiple_of(WORD) {
        return Err(
            "HotSpot pointers are not 8 bytes wide, so this JVM was not walked".to_string(),
        );
    }
    let code_range = match major {
        17 | 21 => pointer_code_range(
            instance("CodeBlob", "_code_begin")?,
            instance("CodeBlob", "_code_end")?,
        )?,
        25 => {
            if fields.contains_key(&("CodeBlob".to_string(), "_code_begin".to_string()))
                || fields.contains_key(&("CodeBlob".to_string(), "_code_end".to_string()))
            {
                return Err(
                    "HotSpot 25 exported stored code pointers, which this build does not walk"
                        .to_string(),
                );
            }
            offset_code_range(
                instance("CodeBlob", "_code_offset")?,
                instance("CodeBlob", "_data_offset")?,
            )?
        }
        _ => {
            return Err(format!(
                "HotSpot {major} is not a walked LTS (17, 21, or 25), so Java methods were not walked"
            ));
        }
    };
    let method_words = interpreter_slot(read_frame_constants(read, &resolved)?, arch, major)?;
    let method_type = compiled_method_type(major)?;
    Ok(JavaVm {
        layout: Layout {
            code_range,
            frame_size: instance("CodeBlob", "_frame_size")?,
            blob_size: instance("CodeBlob", "_size")?,
            blob_name: instance("CodeBlob", "_name")?,
            compiled_method: instance(method_type, "_method")?,
            const_method: instance("Method", "_constMethod")?,
            constants: instance("ConstMethod", "_constants")?,
            name_index: instance("ConstMethod", "_name_index")?,
            pool_holder: instance("ConstantPool", "_pool_holder")?,
            klass_name: instance("Klass", "_name")?,
            sym_length: instance("Symbol", "_length")?,
            sym_body: instance("Symbol", "_body")?,
            heap_memory: instance("CodeHeap", "_memory")?,
            heap_segmap: instance("CodeHeap", "_segmap")?,
            heap_log2: instance("CodeHeap", "_log2_segment_size")?,
            vs_low,
            vs_high: instance("VirtualSpace", "_high")?,
            heap_block,
            heap_used,
            array_len,
            array_cap,
            array_data,
            cp_size,
            method_words,
        },
        heaps: static_addr("CodeCache", "_heaps")?,
        low_bound: static_addr("CodeCache", "_low_bound")?,
        high_bound: static_addr("CodeCache", "_high_bound")?,
    })
}

fn read_type_sizes<R>(
    read: &mut R,
    sym: &HashMap<&str, u64>,
) -> Result<HashMap<String, u64>, String>
where
    R: FnMut(u64, &mut [u8]) -> bool,
{
    let stride = sym["gHotSpotVMTypeEntryArrayStride"];
    let name_off = sym["gHotSpotVMTypeEntryTypeNameOffset"];
    let size_off = sym["gHotSpotVMTypeEntrySizeOffset"];
    let types = sym["gHotSpotVMTypes"];
    if !(8..256).contains(&stride) {
        return Err(format!(
            "HotSpot type entry stride is {stride}, which this build does not walk"
        ));
    }
    let mut sizes = HashMap::new();
    for index in 0..MAX_TYPES {
        let entry = types
            .checked_add(index as u64 * stride)
            .ok_or_else(|| "HotSpot type table is too large to walk".to_string())?;
        let name_ptr = read_u64(read, entry.checked_add(name_off).unwrap_or(0)).unwrap_or(0);
        if name_ptr == 0 {
            break;
        }
        let Some(name) = read_cstr(read, name_ptr, 128) else {
            continue;
        };
        if matches!(
            name.as_str(),
            "HeapBlock" | "GrowableArrayBase" | "ConstantPool"
        ) {
            let size = read_u64(read, entry.checked_add(size_off).unwrap_or(0)).unwrap_or(0);
            sizes.insert(name, size);
        }
    }
    Ok(sizes)
}

/// CPU of the libjvm image that was actually mapped, not of this process.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum JvmArch {
    X86_64,
    Aarch64,
}

impl JvmArch {
    fn label(self) -> &'static str {
        match self {
            Self::X86_64 => "x86_64",
            Self::Aarch64 => "aarch64",
        }
    }
}

/// `Abstract_VM_Version._vm_major_version`, which HotSpot sets from
/// `VERSION_FEATURE`. Only 17, 21, and 25 are walked.
fn hotspot_major<R>(read: &mut R, address: u64) -> Result<i32, String>
where
    R: FnMut(u64, &mut [u8]) -> bool,
{
    let major = read_i32(read, address).ok_or_else(|| {
        "cannot read Abstract_VM_Version._vm_major_version, so Java methods were not walked"
            .to_string()
    })?;
    if matches!(major, 17 | 21 | 25) {
        Ok(major)
    } else {
        Err(format!(
            "HotSpot {major} is not a walked LTS (17, 21, or 25), so Java methods were not walked"
        ))
    }
}

/// Where a blob's instructions live.
///
/// JDK 17 and 21 store absolute `_code_begin` and `_code_end` pointers. JDK 25
/// stores `_code_offset` and `_data_offset`; `code_begin()` is `this +
/// _code_offset` and `code_end()` is `this + _data_offset`.
#[derive(Clone, Copy, Debug)]
enum CodeRange {
    Pointers { begin: u64, end: u64 },
    Offsets { code: u64, data: u64 },
}

fn pointer_code_range(begin: u64, end: u64) -> Result<CodeRange, String> {
    if begin.is_multiple_of(WORD) && end.is_multiple_of(WORD) && begin != end {
        Ok(CodeRange::Pointers { begin, end })
    } else {
        Err(format!(
            "CodeBlob code pointers are not 8-byte fields (begin {begin}, end {end}), so Java methods were not walked"
        ))
    }
}

fn offset_code_range(code: u64, data: u64) -> Result<CodeRange, String> {
    if code < 4096 && data < 4096 && code != data {
        Ok(CodeRange::Offsets { code, data })
    } else {
        Err(format!(
            "HotSpot 25 code offsets are not the product header (code {code}, data {data}), so Java methods were not walked"
        ))
    }
}

fn code_bounds<R>(blob: u64, size: i32, range: CodeRange, read: &mut R) -> Option<(u64, u64)>
where
    R: FnMut(u64, &mut [u8]) -> bool,
{
    match range {
        CodeRange::Pointers { begin, end } => {
            let begin = read_u64(read, blob.checked_add(begin)?)?;
            let end = read_u64(read, blob.checked_add(end)?)?;
            (begin < end).then_some((begin, end))
        }
        CodeRange::Offsets { code, data } => {
            let code_off = read_i32(read, blob.checked_add(code)?)?;
            let data_off = read_i32(read, blob.checked_add(data)?)?;
            if code_off <= 0 || data_off <= code_off || data_off > size {
                return None;
            }
            Some((
                blob.checked_add(u64::try_from(code_off).ok()?)?,
                blob.checked_add(u64::try_from(data_off).ok()?)?,
            ))
        }
    }
}

/// Used-byte offset of the product `HeapBlock` for this major version.
///
/// The numbers have to be the ones the struct table exported. JDK 17 and 21
/// pad `size_t _length` plus `bool _used` to 16 bytes, so `_used` is at 8.
/// JDK 25 changed the length to `uint32_t` and pads the header to 8 bytes, so
/// `_used` is at 4. `allocated_space()` is `this + 1`, which is `size`.
fn heap_used_offset(
    major: i32,
    size: u64,
    header: u64,
    length: u64,
    used: u64,
) -> Result<u64, String> {
    let (want_size, want_used) = match major {
        17 | 21 => (16, 8),
        25 => (8, 4),
        _ => {
            return Err(format!(
                "HotSpot {major} is not a walked LTS (17, 21, or 25), so Java methods were not walked"
            ));
        }
    };
    let used_at = header.checked_add(used).ok_or_else(|| {
        "HeapBlock used offset overflowed, so Java methods were not walked".to_string()
    })?;
    if size == want_size && header == 0 && length == 0 && used == want_used && used_at < size {
        return Ok(used_at);
    }
    Err(format!(
        "HeapBlock for HotSpot {major} is size {size}, header {header}, length {length}, used {used}; this build walks the product header (16 bytes, used at 8 on 17 and 21; 8 bytes, used at 4 on 25)"
    ))
}

/// Product code-cache array: two ints, then the data pointer.
///
/// JDK 17 names the second int `_max`. JDK 21 and 25 name it `_capacity`.
/// The offsets are the same. `GrowableArray<int>._data` is the pointer used
/// for every element type, because it sits at the end of the 8-byte base.
fn array_offsets(
    major: i32,
    size: u64,
    len: u64,
    cap: u64,
    data: u64,
) -> Result<(u64, u64, u64), String> {
    if matches!(major, 17 | 21 | 25) && size == 8 && len == 0 && cap == 4 && data == 8 {
        return Ok((len, cap, data));
    }
    Err(format!(
        "GrowableArray for HotSpot {major} is size {size}, _len {len}, capacity {cap}, _data {data}; this build walks the 8-byte product layout with the data pointer at 8"
    ))
}

/// Type that holds the compiled `Method*`.
///
/// JDK 25 removed `CompiledMethod` and exports `nmethod._method`.
fn compiled_method_type(major: i32) -> Result<&'static str, String> {
    match major {
        17 | 21 => Ok("CompiledMethod"),
        25 => Ok("nmethod"),
        _ => Err(format!(
            "HotSpot {major} is not a walked LTS (17, 21, or 25), so Java methods were not walked"
        )),
    }
}

/// Interpreter slot from the serviceability table, or absent.
#[derive(Clone, Copy)]
enum InterpreterLayout {
    Exported { sender_sp: i32, last_sp: i32 },
    Unexported,
}

fn read_frame_constants<R>(
    read: &mut R,
    sym: &HashMap<&str, u64>,
) -> Result<InterpreterLayout, String>
where
    R: FnMut(u64, &mut [u8]) -> bool,
{
    let stride = sym["gHotSpotVMIntConstantEntryArrayStride"];
    let name_off = sym["gHotSpotVMIntConstantEntryNameOffset"];
    let value_off = sym["gHotSpotVMIntConstantEntryValueOffset"];
    let table = sym["gHotSpotVMIntConstants"];
    if !(8..256).contains(&stride) {
        return Err(format!(
            "HotSpot int-constant stride is {stride}, which this build does not walk"
        ));
    }
    let mut sender_sp = None;
    let mut last_sp = None;
    let mut saw_end = false;
    for index in 0..MAX_CONSTANTS {
        let entry = table
            .checked_add(index as u64 * stride)
            .ok_or_else(|| "HotSpot int-constant table is too large to walk".to_string())?;
        let name_ptr = read_u64(read, entry.checked_add(name_off).unwrap_or(0)).unwrap_or(0);
        if name_ptr == 0 {
            saw_end = true;
            break;
        }
        let Some(name) = read_cstr(read, name_ptr, 160) else {
            continue;
        };
        let value = read_i32(read, entry.checked_add(value_off).unwrap_or(0)).unwrap_or(0);
        if name == "frame::interpreter_frame_sender_sp_offset" {
            sender_sp = Some(value);
        } else if name == "frame::interpreter_frame_last_sp_offset" {
            last_sp = Some(value);
        }
        if sender_sp.is_some() && last_sp.is_some() {
            break;
        }
    }
    match (sender_sp, last_sp, saw_end) {
        (Some(sender_sp), Some(last_sp), _) => {
            Ok(InterpreterLayout::Exported { sender_sp, last_sp })
        }
        (None, None, true) => Ok(InterpreterLayout::Unexported),
        (None, None, false) => Err(
            "HotSpot int-constant table did not end, so Java methods were not walked".to_string(),
        ),
        _ => Err(
            "HotSpot exported only one interpreter frame constant, so Java methods were not walked"
                .to_string(),
        ),
    }
}

/// Method slot from an exported pair, or from the checked aarch64 header.
///
/// An exported pair is accepted only when it is `sender_sp = -1` and
/// `last_sp = -2`. An empty table is accepted only for an aarch64 libjvm
/// whose major version is 17, 21, or 25: those three `frame_aarch64.hpp`
/// headers define that pair and their `VM_INT_CONSTANTS_CPU` is empty. x86_64
/// exports the pair in those releases, so a missing pair there is refused.
fn interpreter_slot(layout: InterpreterLayout, arch: JvmArch, major: i32) -> Result<i32, String> {
    match layout {
        InterpreterLayout::Exported { sender_sp, last_sp } => {
            interpreter_method_words(sender_sp, last_sp).ok_or_else(|| {
                format!(
                    "HotSpot interpreter frame layout is not the supported one (sender_sp {sender_sp}, last_sp {last_sp}); Java methods were not walked"
                )
            })
        }
        InterpreterLayout::Unexported => {
            if arch == JvmArch::Aarch64 && matches!(major, 17 | 21 | 25) {
                Ok(-3)
            } else {
                Err(format!(
                    "HotSpot {} {major} did not export the interpreter frame constants, so Java methods were not walked",
                    arch.label()
                ))
            }
        }
    }
}

/// `method = last_sp - 1` on the JDK 17, 21, and 25 x86_64 and aarch64 headers.
///
/// Only the exported pair `sender_sp = -1`, `last_sp = -2` is accepted. A
/// different pair means the slot moved and this build will not guess it.
fn interpreter_method_words(sender_sp: i32, last_sp: i32) -> Option<i32> {
    if sender_sp == -1 && last_sp == -2 {
        last_sp.checked_sub(1)
    } else {
        None
    }
}

fn compiled_sender<R>(sp: u64, frame_words: i32, read: &mut R) -> Option<(u64, u64, u64)>
where
    R: FnMut(u64, &mut [u8]) -> bool,
{
    if !(0 < frame_words && frame_words < MAX_FRAME_WORDS) || !sp.is_multiple_of(WORD) {
        return None;
    }
    let sender_sp = sp.checked_add(u64::try_from(frame_words).ok()?.checked_mul(WORD)?)?;
    if sender_sp <= sp {
        return None;
    }
    // Return address at sender_sp - 1 word. Saved FP at sender_sp - 2 words
    // (`frame::sender_sp_offset`), on both x86_64 and aarch64 for this layout.
    let pc = canonical_user(read_u64(read, sender_sp.checked_sub(WORD)?)?)?;
    let saved = read_u64(read, sender_sp.checked_sub(2 * WORD)?).unwrap_or(0);
    let next_fp = canonical_user(saved).unwrap_or(0);
    Some((pc, sender_sp, next_fp))
}

fn interpreter_sender<R>(sp: u64, fp: u64, read: &mut R) -> Option<(u64, u64, u64)>
where
    R: FnMut(u64, &mut [u8]) -> bool,
{
    if fp <= sp || !fp.is_multiple_of(WORD) || fp - sp > 256 * 1024 {
        return None;
    }
    let sender_sp = canonical_user(read_u64(read, word_addr(fp, -1)?)?)?;
    let sender_fp = canonical_user(read_u64(read, fp)?)?;
    let sender_pc = canonical_user(read_u64(read, word_addr(fp, 1)?)?)?;
    if sender_sp <= sp || sender_fp <= fp {
        return None;
    }
    Some((sender_pc, sender_sp, sender_fp))
}

fn word_addr(base: u64, words: i32) -> Option<u64> {
    let bytes = i64::from(words).checked_mul(i64::try_from(WORD).ok()?)?;
    if bytes >= 0 {
        base.checked_add(u64::try_from(bytes).ok()?)
    } else {
        base.checked_sub(u64::try_from(-bytes).ok()?)
    }
}

fn symbol<R>(read: &mut R, sym: u64, layout: &Layout) -> Option<String>
where
    R: FnMut(u64, &mut [u8]) -> bool,
{
    if sym < 0x1_0000 || !sym.is_multiple_of(WORD) {
        return None;
    }
    let len = read_u16(read, sym.checked_add(layout.sym_length)?)?;
    if len == 0 || usize::from(len) > MAX_NAME {
        return None;
    }
    let mut body = vec![0u8; usize::from(len)];
    if !read(sym.checked_add(layout.sym_body)?, &mut body) {
        return None;
    }
    let text = String::from_utf8(body).ok()?;
    if text.chars().any(|ch| ch.is_control()) {
        return None;
    }
    Some(text)
}

fn canonical_user(addr: u64) -> Option<u64> {
    if plausible_user(addr) {
        return Some(addr);
    }
    // Apple Silicon return addresses can carry a PAC in the top byte.
    // x86_64 pointers are not authenticated; masking one would turn a
    // kernel address into a user-looking value.
    #[cfg(target_arch = "aarch64")]
    {
        let stripped = addr & 0x00ff_ffff_ffff_ffff;
        if plausible_user(stripped) {
            return Some(stripped);
        }
    }
    None
}

fn plausible_user(addr: u64) -> bool {
    (0x1_0000..0x0000_8000_0000_0000).contains(&addr)
}

fn read_u64<R>(read: &mut R, addr: u64) -> Option<u64>
where
    R: FnMut(u64, &mut [u8]) -> bool,
{
    let mut buf = [0u8; 8];
    read(addr, &mut buf).then_some(u64::from_le_bytes(buf))
}

fn read_u16<R>(read: &mut R, addr: u64) -> Option<u16>
where
    R: FnMut(u64, &mut [u8]) -> bool,
{
    let mut buf = [0u8; 2];
    read(addr, &mut buf).then_some(u16::from_le_bytes(buf))
}

fn read_i32<R>(read: &mut R, addr: u64) -> Option<i32>
where
    R: FnMut(u64, &mut [u8]) -> bool,
{
    let mut buf = [0u8; 4];
    read(addr, &mut buf).then_some(i32::from_le_bytes(buf))
}

fn read_cstr<R>(read: &mut R, addr: u64, max: usize) -> Option<String>
where
    R: FnMut(u64, &mut [u8]) -> bool,
{
    if addr < 0x1_0000 || max == 0 {
        return None;
    }
    let mut buf = vec![0u8; max];
    if !read(addr, &mut buf) {
        return None;
    }
    let end = buf.iter().position(|byte| *byte == 0)?;
    if end == 0 {
        return None;
    }
    String::from_utf8(buf[..end].to_vec()).ok()
}

fn is_libjvm(path: &str) -> bool {
    let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
    let name = name.strip_suffix(" (deleted)").unwrap_or(name);
    let lower = name.to_ascii_lowercase();
    lower == "libjvm.so" || lower == "libjvm.dylib" || lower == "jvm.dll"
}

#[cfg(target_os = "linux")]
fn linux_has_libjvm(pid: u32) -> bool {
    linux_libjvm(pid).is_ok()
}

#[cfg(target_os = "linux")]
fn linux_libjvm(pid: u32) -> Result<(PathBuf, Vec<crate::maps::MapSpan>), String> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/maps"))
        .map_err(|_| format!("pid {pid} is not a HotSpot process (its maps could not be read)"))?;
    let spans = crate::maps::parse_maps(&text);
    let path = spans
        .iter()
        .map(|span| crate::maps::live_path(&span.path))
        .find(|path| is_libjvm(path))
        .ok_or_else(|| format!("pid {pid} is not a HotSpot process (libjvm was not found)"))?;
    let path = PathBuf::from(path);
    let owned = spans
        .into_iter()
        .filter(|span| Path::new(crate::maps::live_path(&span.path)) == path)
        .collect();
    Ok((path, owned))
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn probe_linux(pid: u32, addr: u64) -> Result<(), String> {
    let mut buf = [0u8; 8];
    linux_read(pid, addr, &mut buf).map_err(|err| linux_denied(pid, err))
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn linux_read(pid: u32, addr: u64, buf: &mut [u8]) -> Result<(), i32> {
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

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn linux_denied(pid: u32, err: i32) -> String {
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
            "cannot read Java methods of pid {pid}: process_vm_readv failed with errno {err} ({os}). {scope}. Reading a HotSpot frame uses ptrace attach permission. perf_event_open permission is not enough. An ancestor of the target, root, or scope 0 can read it. anyprof does not change this sysctl"
        )
    } else {
        format!(
            "cannot read Java methods of pid {pid}: process_vm_readv failed with errno {err} ({os}). {scope}. The profile was not recorded"
        )
    }
}

#[cfg(target_os = "linux")]
struct ElfHotspot {
    arch: JvmArch,
    load_offset: u64,
    load_vaddr: u64,
    values: HashMap<String, u64>,
}

#[cfg(target_os = "linux")]
fn elf_hotspot(path: &Path) -> Result<ElfHotspot, String> {
    let mut file = std::fs::File::open(path).map_err(|_| {
        format!(
            "libjvm at {} could not be opened, so Java methods were not read",
            path.display()
        )
    })?;
    use std::io::{Read, Seek, SeekFrom};
    let mut ident = [0u8; 16];
    file.read_exact(&mut ident).map_err(|_| elf_reject(path))?;
    if ident[0..4] != [0x7f, b'E', b'L', b'F'] || ident[4] != 2 || ident[5] != 1 {
        return Err(elf_reject(path));
    }
    let mut ehdr = [0u8; 64];
    file.seek(SeekFrom::Start(0))
        .map_err(|_| elf_reject(path))?;
    file.read_exact(&mut ehdr).map_err(|_| elf_reject(path))?;
    let machine = u16::from_le_bytes(ehdr[18..20].try_into().unwrap());
    let arch = match machine {
        62 => JvmArch::X86_64,
        183 => JvmArch::Aarch64,
        _ => {
            return Err(format!(
                "libjvm at {} has ELF machine {machine}, which this build does not walk",
                path.display()
            ));
        }
    };
    let phoff = u64::from_le_bytes(ehdr[32..40].try_into().unwrap());
    let shoff = u64::from_le_bytes(ehdr[40..48].try_into().unwrap());
    let phentsize = u16::from_le_bytes(ehdr[54..56].try_into().unwrap());
    let phnum = u16::from_le_bytes(ehdr[56..58].try_into().unwrap());
    let shentsize = u16::from_le_bytes(ehdr[58..60].try_into().unwrap());
    let shnum = u16::from_le_bytes(ehdr[60..62].try_into().unwrap());
    if phentsize < 56 || shentsize < 64 || phnum > 128 || shnum > 10_000 {
        return Err(elf_reject(path));
    }
    let mut load_offset = None;
    let mut load_vaddr = None;
    for index in 0..phnum {
        let mut ph = vec![0u8; usize::from(phentsize)];
        let at = phoff
            .checked_add(u64::from(index) * u64::from(phentsize))
            .ok_or_else(|| elf_reject(path))?;
        file.seek(SeekFrom::Start(at))
            .map_err(|_| elf_reject(path))?;
        file.read_exact(&mut ph).map_err(|_| elf_reject(path))?;
        let kind = u32::from_le_bytes(ph[0..4].try_into().unwrap());
        if kind != 1 {
            continue;
        }
        let offset = u64::from_le_bytes(ph[8..16].try_into().unwrap());
        let vaddr = u64::from_le_bytes(ph[16..24].try_into().unwrap());
        if load_vaddr.is_none_or(|current| vaddr < current) {
            load_offset = Some(offset);
            load_vaddr = Some(vaddr);
        }
    }
    let (Some(load_offset), Some(load_vaddr)) = (load_offset, load_vaddr) else {
        return Err(elf_reject(path));
    };
    let mut sections = Vec::with_capacity(usize::from(shnum));
    for index in 0..shnum {
        let mut sh = vec![0u8; usize::from(shentsize)];
        let at = shoff
            .checked_add(u64::from(index) * u64::from(shentsize))
            .ok_or_else(|| elf_reject(path))?;
        file.seek(SeekFrom::Start(at))
            .map_err(|_| elf_reject(path))?;
        file.read_exact(&mut sh).map_err(|_| elf_reject(path))?;
        sections.push(sh);
    }
    let mut wanted: HashMap<String, u64> = HashMap::new();
    for kind in [11u32, 2] {
        if wanted.len() == HOTSPOT_SYMBOLS.len() {
            break;
        }
        for section in &sections {
            let section_type = u32::from_le_bytes(section[4..8].try_into().unwrap());
            if section_type != kind {
                continue;
            }
            let offset = u64::from_le_bytes(section[24..32].try_into().unwrap());
            let size = u64::from_le_bytes(section[32..40].try_into().unwrap());
            let link = u32::from_le_bytes(section[40..44].try_into().unwrap());
            let entsize = u64::from_le_bytes(section[56..64].try_into().unwrap());
            if entsize < 24 || size > 64 * 1024 * 1024 || link as usize >= sections.len() {
                continue;
            }
            let strings = &sections[link as usize];
            let str_off = u64::from_le_bytes(strings[24..32].try_into().unwrap());
            let str_size = u64::from_le_bytes(strings[32..40].try_into().unwrap());
            if str_size > 64 * 1024 * 1024 {
                continue;
            }
            let mut table = vec![0u8; size as usize];
            file.seek(SeekFrom::Start(offset))
                .map_err(|_| elf_reject(path))?;
            file.read_exact(&mut table).map_err(|_| elf_reject(path))?;
            let mut names = vec![0u8; str_size as usize];
            file.seek(SeekFrom::Start(str_off))
                .map_err(|_| elf_reject(path))?;
            file.read_exact(&mut names).map_err(|_| elf_reject(path))?;
            let count = (size / entsize) as usize;
            for index in 0..count {
                let at = index * entsize as usize;
                let name_off = u32::from_le_bytes(table[at..at + 4].try_into().unwrap()) as usize;
                let shndx = u16::from_le_bytes(table[at + 6..at + 8].try_into().unwrap());
                let value = u64::from_le_bytes(table[at + 8..at + 16].try_into().unwrap());
                if shndx == 0 || name_off >= names.len() {
                    continue;
                }
                let end = names[name_off..]
                    .iter()
                    .position(|byte| *byte == 0)
                    .map(|pos| name_off + pos)
                    .unwrap_or(names.len());
                let Ok(name) = std::str::from_utf8(&names[name_off..end]) else {
                    continue;
                };
                if HOTSPOT_SYMBOLS.contains(&name) && !wanted.contains_key(name) {
                    wanted.insert(name.to_string(), value);
                }
            }
        }
    }
    if let Some(missing) = HOTSPOT_SYMBOLS
        .iter()
        .find(|name| !wanted.contains_key(**name))
    {
        return Err(format!(
            "libjvm at {} does not export {missing}, so Java methods cannot be read",
            path.display()
        ));
    }
    Ok(ElfHotspot {
        arch,
        load_offset,
        load_vaddr,
        values: wanted,
    })
}

#[cfg(target_os = "linux")]
fn elf_reject(path: &Path) -> String {
    format!(
        "libjvm at {} is not a 64-bit little-endian ELF, so Java methods were not read",
        path.display()
    )
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn elf_slide(
    spans: &[crate::maps::MapSpan],
    path: &Path,
    load_offset: u64,
    load_vaddr: u64,
) -> Option<u64> {
    spans
        .iter()
        .find(|span| {
            Path::new(crate::maps::live_path(&span.path)) == path && span.offset == load_offset
        })
        .map(|span| span.start.wrapping_sub(load_vaddr))
}

/// CPU of the Mach-O header mapped at `load`, not of this process.
#[cfg(target_os = "macos")]
fn mapped_mach_arch<R>(load: u64, read: &mut R) -> Result<JvmArch, String>
where
    R: FnMut(u64, &mut [u8]) -> bool,
{
    let mut header = [0u8; 8];
    if !read(load, &mut header) {
        return Err(
            "cannot read the mapped libjvm Mach-O header, so Java methods were not walked"
                .to_string(),
        );
    }
    let magic = u32::from_le_bytes(header[0..4].try_into().unwrap());
    let cputype = u32::from_le_bytes(header[4..8].try_into().unwrap());
    // MH_MAGIC_64. A fat header is not what dyld maps.
    if magic != 0xfeed_facf {
        return Err(format!(
            "mapped libjvm magic is {magic:#x}, so Java methods were not walked"
        ));
    }
    match cputype {
        0x0100_0007 => Ok(JvmArch::X86_64),
        0x0100_000C => Ok(JvmArch::Aarch64),
        _ => Err(format!(
            "mapped libjvm cputype is {cputype:#x}, so Java methods were not walked"
        )),
    }
}

#[cfg(target_os = "macos")]
fn macho_cpu(arch: JvmArch) -> crate::macho::MachCpu {
    match arch {
        JvmArch::Aarch64 => crate::macho::MachCpu::Arm64,
        JvmArch::X86_64 => crate::macho::MachCpu::X86_64,
    }
}

#[cfg(target_os = "macos")]
fn macho_hotspot(path: &Path, load: u64, arch: JvmArch) -> Result<HashMap<String, u64>, String> {
    let module = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("libjvm.dylib");
    let image = crate::macho::ImageSymbols::parse_file(path, load, module, macho_cpu(arch))
        .ok_or_else(|| {
            format!(
                "libjvm at {} has no Mach-O symbol table, so Java methods cannot be read",
                path.display()
            )
        })?;
    let mut values = HashMap::new();
    for name in HOTSPOT_SYMBOLS {
        let Some(addr) = image.address_of(name) else {
            return Err(format!(
                "libjvm at {} does not export {name}, so Java methods cannot be read",
                path.display()
            ));
        };
        values.insert((*name).to_string(), addr);
    }
    Ok(values)
}

#[cfg(target_os = "macos")]
fn macos_has_libjvm(pid: u32) -> bool {
    if macos_regions_have_libjvm(pid) {
        return true;
    }
    macos_exe_is_java(pid)
}

#[cfg(target_os = "macos")]
fn macos_regions_have_libjvm(pid: u32) -> bool {
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    let mut address = 0u64;
    let mut buf = vec![0u8; 8192];
    for _ in 0..100_000 {
        let wrote =
            unsafe { proc_pidinfo(pid, 8, address, buf.as_mut_ptr().cast(), buf.len() as i32) };
        if wrote <= 0 || wrote as usize > buf.len() {
            return false;
        }
        if buf[..wrote as usize]
            .windows(b"libjvm.dylib".len())
            .any(|window| window == b"libjvm.dylib")
        {
            return true;
        }
        if (wrote as usize) < 96 {
            return false;
        }
        let region = u64::from_le_bytes(buf[80..88].try_into().unwrap());
        let size = u64::from_le_bytes(buf[88..96].try_into().unwrap());
        if size == 0 {
            return false;
        }
        let Some(next) = region.checked_add(size) else {
            return false;
        };
        if next <= address {
            return false;
        }
        address = next;
    }
    false
}

#[cfg(target_os = "macos")]
fn macos_exe_is_java(pid: u32) -> bool {
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    let mut buf = vec![0u8; 4 * 1024];
    let n = unsafe { proc_pidpath(pid, buf.as_mut_ptr().cast(), buf.len() as u32) };
    if n <= 0 {
        return false;
    }
    let end = buf.iter().position(|byte| *byte == 0).unwrap_or(n as usize);
    let Ok(text) = std::str::from_utf8(&buf[..end]) else {
        return false;
    };
    let name = text.rsplit('/').next().unwrap_or(text);
    name.eq_ignore_ascii_case("java") || name.eq_ignore_ascii_case("javaw")
}

#[cfg(target_os = "macos")]
#[link(name = "proc")]
unsafe extern "C" {
    fn proc_pidinfo(
        pid: i32,
        flavor: i32,
        arg: u64,
        buffer: *mut std::ffi::c_void,
        buffersize: i32,
    ) -> i32;
    fn proc_pidpath(pid: i32, buffer: *mut std::ffi::c_char, buffersize: u32) -> i32;
}

#[cfg(all(windows, target_arch = "x86_64"))]
fn read_windows(
    process: windows_sys::Win32::Foundation::HANDLE,
    addr: u64,
    buf: &mut [u8],
) -> bool {
    if addr == 0 || buf.is_empty() {
        return false;
    }
    let mut read = 0usize;
    let ok = unsafe {
        windows_sys::Win32::System::Diagnostics::Debug::ReadProcessMemory(
            process,
            addr as *const core::ffi::c_void,
            buf.as_mut_ptr().cast(),
            buf.len(),
            &mut read,
        )
    };
    ok != 0 && read == buf.len()
}

#[cfg(all(windows, target_arch = "x86_64"))]
fn windows_has_libjvm(pid: u32) -> bool {
    crate::windows::with_process(pid, |process| windows_libjvm(process).map(|_| ())).is_ok()
}

#[cfg(all(windows, not(target_arch = "x86_64")))]
fn windows_has_libjvm(pid: u32) -> bool {
    let _ = pid;
    false
}

#[cfg(all(windows, target_arch = "x86_64"))]
fn windows_libjvm(
    process: windows_sys::Win32::Foundation::HANDLE,
) -> Result<(PathBuf, u64), String> {
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
        return Err("the process module list could not be read".to_string());
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
        if !is_libjvm(&path.to_string_lossy()) {
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
        if ok == 0 || info.lpBaseOfDll.is_null() {
            continue;
        }
        return Ok((path, info.lpBaseOfDll as u64));
    }
    Err("jvm.dll was not loaded".to_string())
}

#[cfg(all(windows, target_arch = "x86_64"))]
fn pe_hotspot(path: &Path, base: u64) -> Result<(HashMap<String, u64>, JvmArch), String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path).map_err(|_| {
        format!(
            "jvm.dll at {} could not be opened, so Java methods were not read",
            path.display()
        )
    })?;
    let mut dos = [0u8; 64];
    file.read_exact(&mut dos).map_err(|_| pe_reject(path))?;
    if dos[0] != b'M' || dos[1] != b'Z' {
        return Err(pe_reject(path));
    }
    let e_lfanew = u32::from_le_bytes(dos[0x3c..0x40].try_into().unwrap());
    if e_lfanew > 1024 * 1024 {
        return Err(pe_reject(path));
    }
    file.seek(SeekFrom::Start(u64::from(e_lfanew)))
        .map_err(|_| pe_reject(path))?;
    let mut pe = [0u8; 24];
    file.read_exact(&mut pe).map_err(|_| pe_reject(path))?;
    if &pe[0..4] != b"PE\0\0" {
        return Err(pe_reject(path));
    }
    let machine = u16::from_le_bytes(pe[4..6].try_into().unwrap());
    let arch = match machine {
        0x8664 => JvmArch::X86_64,
        0xAA64 => JvmArch::Aarch64,
        _ => {
            return Err(format!(
                "jvm.dll at {} has PE machine {machine:#x}, which this build does not walk",
                path.display()
            ));
        }
    };
    let sections = u16::from_le_bytes(pe[6..8].try_into().unwrap());
    let optional_size = u16::from_le_bytes(pe[20..22].try_into().unwrap());
    if sections > 96 || !(120..=4096).contains(&optional_size) {
        return Err(pe_reject(path));
    }
    let mut optional = vec![0u8; usize::from(optional_size)];
    file.read_exact(&mut optional)
        .map_err(|_| pe_reject(path))?;
    let magic = u16::from_le_bytes(optional[0..2].try_into().unwrap());
    if magic != 0x20b {
        return Err("jvm.dll is not a PE32+ image, so Java methods were not read".to_string());
    }
    let export_rva = u32::from_le_bytes(optional[112..116].try_into().unwrap());
    let export_size = u32::from_le_bytes(optional[116..120].try_into().unwrap());
    if export_rva == 0 || export_size < 40 {
        return Err(format!(
            "jvm.dll at {} has no export table, so Java methods cannot be read",
            path.display()
        ));
    }
    let mut section_table = vec![0u8; usize::from(sections) * 40];
    file.read_exact(&mut section_table)
        .map_err(|_| pe_reject(path))?;
    let rva_offset = |rva: u32| -> Option<u64> {
        let rva = u64::from(rva);
        for index in 0..usize::from(sections) {
            let at = index * 40;
            let virtual_size =
                u32::from_le_bytes(section_table[at + 8..at + 12].try_into().ok()?) as u64;
            let virtual_addr =
                u32::from_le_bytes(section_table[at + 12..at + 16].try_into().ok()?) as u64;
            let raw_size =
                u32::from_le_bytes(section_table[at + 16..at + 20].try_into().ok()?) as u64;
            let raw_ptr =
                u32::from_le_bytes(section_table[at + 20..at + 24].try_into().ok()?) as u64;
            let span = virtual_size.max(raw_size);
            if rva >= virtual_addr && rva - virtual_addr < span {
                return raw_ptr.checked_add(rva - virtual_addr);
            }
        }
        None
    };
    let export_at = rva_offset(export_rva).ok_or_else(|| pe_reject(path))?;
    let mut export = [0u8; 40];
    file.seek(SeekFrom::Start(export_at))
        .map_err(|_| pe_reject(path))?;
    file.read_exact(&mut export).map_err(|_| pe_reject(path))?;
    let function_count = u32::from_le_bytes(export[20..24].try_into().unwrap());
    let name_count = u32::from_le_bytes(export[24..28].try_into().unwrap());
    let functions_rva = u32::from_le_bytes(export[28..32].try_into().unwrap());
    let names_rva = u32::from_le_bytes(export[32..36].try_into().unwrap());
    let ordinals_rva = u32::from_le_bytes(export[36..40].try_into().unwrap());
    if function_count == 0 || name_count == 0 || function_count > 200_000 || name_count > 200_000 {
        return Err(pe_reject(path));
    }
    let mut functions = vec![0u8; function_count as usize * 4];
    let mut names = vec![0u8; name_count as usize * 4];
    let mut ordinals = vec![0u8; name_count as usize * 2];
    file.seek(SeekFrom::Start(
        rva_offset(functions_rva).ok_or_else(|| pe_reject(path))?,
    ))
    .map_err(|_| pe_reject(path))?;
    file.read_exact(&mut functions)
        .map_err(|_| pe_reject(path))?;
    file.seek(SeekFrom::Start(
        rva_offset(names_rva).ok_or_else(|| pe_reject(path))?,
    ))
    .map_err(|_| pe_reject(path))?;
    file.read_exact(&mut names).map_err(|_| pe_reject(path))?;
    file.seek(SeekFrom::Start(
        rva_offset(ordinals_rva).ok_or_else(|| pe_reject(path))?,
    ))
    .map_err(|_| pe_reject(path))?;
    file.read_exact(&mut ordinals)
        .map_err(|_| pe_reject(path))?;
    let mut values = HashMap::new();
    for index in 0..name_count as usize {
        let name_rva = u32::from_le_bytes(names[index * 4..index * 4 + 4].try_into().unwrap());
        let ordinal = u16::from_le_bytes(ordinals[index * 2..index * 2 + 2].try_into().unwrap());
        if u32::from(ordinal) >= function_count {
            continue;
        }
        let function_rva = u32::from_le_bytes(
            functions[ordinal as usize * 4..ordinal as usize * 4 + 4]
                .try_into()
                .unwrap(),
        );
        let name_at = rva_offset(name_rva).ok_or_else(|| pe_reject(path))?;
        file.seek(SeekFrom::Start(name_at))
            .map_err(|_| pe_reject(path))?;
        let mut name_buf = [0u8; 128];
        let n = file.read(&mut name_buf).map_err(|_| pe_reject(path))?;
        let end = name_buf[..n]
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(n);
        let Ok(name) = std::str::from_utf8(&name_buf[..end]) else {
            continue;
        };
        if !HOTSPOT_SYMBOLS.contains(&name) {
            continue;
        }
        let export_start = u64::from(export_rva);
        let export_end = export_start + u64::from(export_size);
        let function = u64::from(function_rva);
        if function >= export_start && function < export_end {
            continue;
        }
        values.insert(name.to_string(), base.wrapping_add(function));
    }
    if let Some(missing) = HOTSPOT_SYMBOLS
        .iter()
        .find(|name| !values.contains_key(**name))
    {
        return Err(format!(
            "jvm.dll at {} does not export {missing}, so Java methods cannot be read",
            path.display()
        ));
    }
    Ok((values, arch))
}

#[cfg(all(windows, target_arch = "x86_64"))]
fn pe_reject(path: &Path) -> String {
    format!(
        "jvm.dll at {} is not a readable PE32+ image, so Java methods were not read",
        path.display()
    )
}

#[cfg(test)]
pub(crate) mod tests {
    use super::{
        InterpreterLayout, JvmArch, array_offsets, compiled_method_type, heap_used_offset,
        interpreter_method_words, interpreter_slot,
    };

    #[test]
    fn interpreter_slot_uses_the_header_only_for_checked_aarch64_lts() {
        assert_eq!(interpreter_method_words(-1, -2), Some(-3));
        assert_eq!(interpreter_method_words(-1, -3), None);
        assert_eq!(interpreter_method_words(0, -2), None);
        let exported = InterpreterLayout::Exported {
            sender_sp: -1,
            last_sp: -2,
        };
        assert_eq!(
            interpreter_slot(exported, JvmArch::X86_64, 21).ok(),
            Some(-3)
        );
        assert_eq!(
            interpreter_slot(exported, JvmArch::Aarch64, 25).ok(),
            Some(-3)
        );
        assert!(
            interpreter_slot(
                InterpreterLayout::Exported {
                    sender_sp: -1,
                    last_sp: -4,
                },
                JvmArch::Aarch64,
                21,
            )
            .is_err()
        );
        for major in [17, 21, 25] {
            assert_eq!(
                interpreter_slot(InterpreterLayout::Unexported, JvmArch::Aarch64, major).ok(),
                Some(-3),
                "aarch64 {major}"
            );
            assert!(
                interpreter_slot(InterpreterLayout::Unexported, JvmArch::X86_64, major).is_err(),
                "x86_64 {major} must export the pair"
            );
        }
        assert!(interpreter_slot(InterpreterLayout::Unexported, JvmArch::Aarch64, 11).is_err());
        assert!(interpreter_slot(InterpreterLayout::Unexported, JvmArch::Aarch64, 22).is_err());
    }

    #[test]
    fn heap_and_array_layouts_match_only_the_checked_product_headers() {
        assert_eq!(heap_used_offset(17, 16, 0, 0, 8).ok(), Some(8));
        assert_eq!(heap_used_offset(21, 16, 0, 0, 8).ok(), Some(8));
        assert_eq!(heap_used_offset(25, 8, 0, 0, 4).ok(), Some(4));
        assert!(heap_used_offset(25, 16, 0, 0, 8).is_err());
        assert!(heap_used_offset(21, 8, 0, 0, 4).is_err());
        assert!(heap_used_offset(22, 16, 0, 0, 8).is_err());
        assert!(heap_used_offset(21, 16, 0, 0, 4).is_err());
        assert_eq!(array_offsets(17, 8, 0, 4, 8).ok(), Some((0, 4, 8)));
        assert_eq!(array_offsets(25, 8, 0, 4, 8).ok(), Some((0, 4, 8)));
        assert!(array_offsets(21, 16, 0, 4, 8).is_err());
        assert!(array_offsets(21, 8, 0, 4, 16).is_err());
        assert_eq!(compiled_method_type(17).ok(), Some("CompiledMethod"));
        assert_eq!(compiled_method_type(21).ok(), Some("CompiledMethod"));
        assert_eq!(compiled_method_type(25).ok(), Some("nmethod"));
        assert!(compiled_method_type(24).is_err());
        assert!(matches!(
            super::pointer_code_range(8, 16),
            Ok(super::CodeRange::Pointers { begin: 8, end: 16 })
        ));
        assert!(super::pointer_code_range(4, 12).is_err());
        assert!(matches!(
            super::offset_code_range(32, 40),
            Ok(super::CodeRange::Offsets { code: 32, data: 40 })
        ));
        assert!(super::offset_code_range(32, 32).is_err());
        assert!(super::offset_code_range(5000, 40).is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn system_libjvm_exports_the_hotspot_table_when_a_jdk_is_installed() {
        let candidates = [
            "/usr/lib/jvm/java-21-openjdk-amd64/lib/server/libjvm.so",
            "/usr/lib/jvm/java-21-openjdk-arm64/lib/server/libjvm.so",
            "/usr/lib/jvm/default-java/lib/server/libjvm.so",
        ];
        let Some(path) = candidates
            .iter()
            .map(std::path::Path::new)
            .find(|path| path.is_file())
        else {
            return;
        };
        let file = super::elf_hotspot(path).expect("libjvm exports");
        assert!(file.values["gHotSpotVMStructs"] > 0);
        assert_eq!(file.values.len(), super::HOTSPOT_SYMBOLS.len());
        let text = path.to_string_lossy();
        if text.contains("amd64") || text.contains("x86_64") {
            assert_eq!(file.arch, super::JvmArch::X86_64);
        }
        if text.contains("arm64") || text.contains("aarch64") {
            assert_eq!(file.arch, super::JvmArch::Aarch64);
        }
    }

    #[test]
    fn synthetic_fixture_unwinds_compiled_frames() {
        let fixture = SyntheticJavaFixture::two_compiled_frames();
        let sp = 0x7fff_0000_1000u64;
        let stack = fixture.build_copied_stack(sp);
        let frames = fixture
            .vm
            .as_ref()
            .expect("vm")
            .frames(fixture.leaf_pc, sp, sp + 16, &mut |addr, buf| {
                if let Some(off) = addr.checked_sub(sp)
                    && let Ok(off) = usize::try_from(off)
                    && let Some(end) = off.checked_add(buf.len())
                    && end <= stack.len()
                {
                    buf.copy_from_slice(&stack[off..end]);
                    return true;
                }
                fixture.read_metadata(addr, buf)
            })
            .expect("compiled java frames");
        assert_eq!(
            frames,
            vec!["Leaf.work".to_string(), "Caller.entry".to_string()]
        );
    }

    pub(crate) struct SyntheticJavaFixture {
        pub vm: Option<super::JavaVm>,
        pub leaf_pc: u64,
        pub caller_pc: u64,
        mem: std::collections::HashMap<u64, u8>,
    }

    impl SyntheticJavaFixture {
        pub fn two_compiled_frames() -> Self {
            use super::{CodeRange, JavaVm, Layout};
            let mut mem = std::collections::HashMap::new();
            let put = |mem: &mut std::collections::HashMap<u64, u8>, addr: u64, bytes: &[u8]| {
                for (idx, &byte) in bytes.iter().enumerate() {
                    mem.insert(addr + idx as u64, byte);
                }
            };
            let put_u64 = |mem: &mut std::collections::HashMap<u64, u8>, addr: u64, val: u64| {
                put(mem, addr, &val.to_le_bytes());
            };
            let put_u16 = |mem: &mut std::collections::HashMap<u64, u8>, addr: u64, val: u16| {
                put(mem, addr, &val.to_le_bytes());
            };
            let put_i32 = |mem: &mut std::collections::HashMap<u64, u8>, addr: u64, val: i32| {
                put(mem, addr, &val.to_le_bytes());
            };
            let put_sym = |mem: &mut std::collections::HashMap<u64, u8>, addr: u64, text: &[u8]| {
                put_u16(mem, addr, text.len() as u16);
                put(mem, addr + 2, text);
            };

            let code_low = 0x3000_0000u64;
            let code_high = 0x3001_0000u64;
            let low_bound_addr = 0x2000_0000u64;
            let high_bound_addr = 0x2000_0008u64;
            let heaps_ptr_addr = 0x2000_0010u64;
            let array_addr = 0x2000_0100u64;
            let array_data_addr = 0x2000_0120u64;
            let heap_addr = 0x2000_0200u64;
            let segmap_addr = 0x2000_1000u64;
            let blob_name_addr = 0x2000_2000u64;

            put_u64(&mut mem, low_bound_addr, code_low);
            put_u64(&mut mem, high_bound_addr, code_high);
            put_u64(&mut mem, heaps_ptr_addr, array_addr);
            put_i32(&mut mem, array_addr, 1);
            put_i32(&mut mem, array_addr + 4, 1);
            put_u64(&mut mem, array_addr + 8, array_data_addr);
            put_u64(&mut mem, array_data_addr, heap_addr);

            // CodeHeap: memory VirtualSpace [0..16], segmap VirtualSpace [16..32], log2_segment_size = 8 (256B)
            put_u64(&mut mem, heap_addr, code_low);
            put_u64(&mut mem, heap_addr + 8, code_high);
            put_u64(&mut mem, heap_addr + 16, segmap_addr);
            put_u64(&mut mem, heap_addr + 24, segmap_addr + 0x400);
            put_i32(&mut mem, heap_addr + 32, 8);
            put(&mut mem, blob_name_addr, b"nmethod\0");

            let add_method = |mem: &mut std::collections::HashMap<u64, u8>,
                              meta_base: u64,
                              cls: &[u8],
                              mth: &[u8]|
             -> u64 {
                let sym_cls = meta_base;
                let sym_mth = meta_base + 0x40;
                let klass = meta_base + 0x80;
                let pool = meta_base + 0xc0;
                let const_method = meta_base + 0x100;
                let method = meta_base + 0x140;
                put_sym(mem, sym_cls, cls);
                put_sym(mem, sym_mth, mth);
                put_u64(mem, klass, sym_cls);
                put_u64(mem, pool, klass);
                put_u64(mem, pool + 16, sym_mth);
                put_u64(mem, const_method, pool);
                put_u16(mem, const_method + 8, 1);
                put_u64(mem, method, const_method);
                method
            };

            let leaf_method = add_method(&mut mem, 0x2001_0000, b"Leaf", b"work");
            let caller_method = add_method(&mut mem, 0x2001_1000, b"Caller", b"entry");

            let add_blob = |mem: &mut std::collections::HashMap<u64, u8>,
                            seg_index: u64,
                            method: u64,
                            frame_words: i32|
             -> u64 {
                let block = code_low + (seg_index << 8);
                put(mem, segmap_addr + seg_index, &[0]);
                put(mem, block + 8, &[1]); // HeapBlock._used = 1
                let blob = block + 16;
                let code_begin = blob + 64;
                let code_end = blob + 192;
                put_u64(mem, blob, code_begin);
                put_u64(mem, blob + 8, code_end);
                put_i32(mem, blob + 16, frame_words);
                put_i32(mem, blob + 20, 192);
                put_u64(mem, blob + 24, blob_name_addr);
                put_u64(mem, blob + 32, method);
                code_begin + 16
            };

            let leaf_pc = add_blob(&mut mem, 1, leaf_method, 4);
            let caller_pc = add_blob(&mut mem, 4, caller_method, 4);

            let vm = JavaVm {
                layout: Layout {
                    code_range: CodeRange::Pointers { begin: 0, end: 8 },
                    frame_size: 16,
                    blob_size: 20,
                    blob_name: 24,
                    compiled_method: 32,
                    const_method: 0,
                    constants: 0,
                    name_index: 8,
                    pool_holder: 0,
                    klass_name: 0,
                    sym_length: 0,
                    sym_body: 2,
                    heap_memory: 0,
                    heap_segmap: 16,
                    heap_log2: 32,
                    vs_low: 0,
                    vs_high: 8,
                    heap_block: 16,
                    heap_used: 8,
                    array_len: 0,
                    array_cap: 4,
                    array_data: 8,
                    cp_size: 8,
                    method_words: -3,
                },
                heaps: heaps_ptr_addr,
                low_bound: low_bound_addr,
                high_bound: high_bound_addr,
            };
            Self {
                vm: Some(vm),
                leaf_pc,
                caller_pc,
                mem,
            }
        }

        pub fn build_copied_stack(&self, sp: u64) -> Vec<u8> {
            // Leaf frame_words = 4 -> sender_sp = sp + 32; return PC at sp + 24, saved FP at sp + 16.
            // Caller frame_words = 4 -> sender_sp = sp + 64; return PC at sp + 56 (outside code cache).
            let mut stack = vec![0u8; 64];
            stack[16..24].copy_from_slice(&(sp + 32).to_le_bytes());
            stack[24..32].copy_from_slice(&self.caller_pc.to_le_bytes());
            stack[48..56].copy_from_slice(&(sp + 64).to_le_bytes());
            stack[56..64].copy_from_slice(&0x5555_0000_9000u64.to_le_bytes());
            stack
        }

        pub fn read_metadata(&self, addr: u64, buf: &mut [u8]) -> bool {
            for (idx, slot) in buf.iter_mut().enumerate() {
                let Some(byte_addr) = addr.checked_add(idx as u64) else {
                    return false;
                };
                let Some(&val) = self.mem.get(&byte_addr) else {
                    return false;
                };
                *slot = val;
            }
            true
        }
    }
}
