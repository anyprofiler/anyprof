//! Go stack traces from the linker’s `pclntab`, not from frame pointers.
//!
//! A Go function is a goroutine frame. The leaf often has no frame pointer
//! (`main.spin` at `-O0` is `nop; jmp`), and the return PC lives where the
//! pcsp table says, which is the calling convention `runtime.gentraceback`
//! uses. Walking saved `%rbp` records stops at that leaf and can invent a
//! caller that is not the goroutine’s caller. This module reads Go 1.18+
//! and Go 1.20+ pclntab (`0xFFFFFFF0` / `0xFFFFFFF1`) and unwinds with pcsp.
//!
//! The sample is still the OS thread that is on CPU. The PC and SP belong
//! to the goroutine that thread is running. A goroutine parked on a channel
//! or in `time.Sleep` is not on that stack, so it is not given an on-CPU
//! sample. A thread inside the scheduler or a syscall shows those runtime
//! frames. This is not a snapshot of `runtime.allgs`.
//!
//! Inlined callees stay inside the outer function. The fixtures are
//! `//go:noinline`. cgo stops at the Go function that entered C, plus the
//! return PC, because the C frames are not in pclntab. Frame-pointer walk
//! remains the fallback when the leaf PC is not a Go function.
//!
//! Names, files, and lines come from the same table, so `-ldflags=-s -w`
//! still symbolizes. The table is the executable’s `.gopclntab` /
//! `__gopclntab`, or, on PE, the blob inside `.rdata` (Go does not give it
//! its own section). Only a file that also contains `\xff Go buildinf:` is
//! scanned on PE, so a C++ image is not parsed looking for a lucky magic.
//!
//! Go 1.18 through 1.25 store the text base in the pclntab header. Go 1.26
//! still uses magic `0xFFFFFFF1` but writes that word as zero. Entry offsets
//! are then relative to `moduledata.text` (`runtime.text`), which the linker
//! puts in `.go.module` / `__go_module`, or in `.data` on PE. A cgo binary’s
//! `runtime.text` is not the first byte of the text section, so that section’s
//! address is not used in its place. An externally linked Mach-O leaves the
//! Go 1.22 header word as an unrelocated addend from the `__TEXT` vmaddr; the
//! link address is that sum when it falls inside `__text`. A value already
//! inside the text section (internal Mach-O, ELF, PE, and Go 1.26’s module
//! record) is the link address and is not shifted again. Go older than 1.18
//! still matches the build-info header and is sampled with the native walk;
//! this parser does not read the 1.2 or 1.16 pclntab layouts.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::Arc;

use crate::walk::{self, MAX_FRAMES};

const GO_1_18: u32 = 0xFFFF_FFF0;
const GO_1_20: u32 = 0xFFFF_FFF1;
const MAX_TABLE: u64 = 32 * 1024 * 1024;
const MAX_FUNCS: u64 = 500_000;
const MAX_NAME: usize = 64 * 1024;
const FUNC_HEAD: usize = 44;
const FLAG_TOP_FRAME: u8 = 1;
const FLAG_SP_WRITE: u8 = 2;
const BUILD_INFO: &[u8] = b"\xff Go buildinf:";

/// amd64 pushes the return PC. arm64 keeps it in LR or in the slot at SP.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) enum GoMachine {
    X86_64,
    Arm64,
}

/// One Go function’s pcsp / file / line offsets.
#[derive(Debug)]
struct FuncRec {
    entry: u32,
    end: u32,
    name_off: u32,
    pcsp: u32,
    pcfile: u32,
    pcln: u32,
    cu_off: u32,
    flag: u8,
}

#[derive(Debug)]
struct GoTables {
    text_start: u64,
    quantum: u32,
    funcs: Vec<FuncRec>,
    data: Vec<u8>,
    funcname: usize,
    cutab: usize,
    filetab: usize,
    pctab: usize,
}

/// A pclntab relocated by `bias` (`runtime - link address`).
#[derive(Debug, Clone)]
pub(crate) struct GoImage {
    tables: Arc<GoTables>,
    runtime_text: u64,
}

/// Function name, file, and line for one program counter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GoSymbol {
    pub name: String,
    pub file: Option<String>,
    pub line: Option<u32>,
}

impl GoImage {
    fn from_tables(tables: GoTables, bias: u64) -> Self {
        let runtime_text = tables.text_start.wrapping_add(bias);
        Self {
            tables: Arc::new(tables),
            runtime_text,
        }
    }

    /// Same table, with `actual_base - preferred_base` as the load bias.
    #[cfg(all(windows, target_arch = "x86_64"))]
    pub(crate) fn at_base(&self, preferred_base: u64, actual_base: u64) -> Self {
        let bias = actual_base.wrapping_sub(preferred_base);
        Self {
            tables: Arc::clone(&self.tables),
            runtime_text: self.tables.text_start.wrapping_add(bias),
        }
    }

    fn text_off(&self, runtime_pc: u64) -> Option<u64> {
        let off = runtime_pc.wrapping_sub(self.runtime_text);
        let limit = self.tables.funcs.last().map(|func| u64::from(func.end))?;
        if off < limit { Some(off) } else { None }
    }

    fn func_at(&self, runtime_pc: u64) -> Option<&FuncRec> {
        let off = self.text_off(runtime_pc)?;
        let off = u32::try_from(off).ok()?;
        let funcs = &self.tables.funcs;
        let idx = funcs.partition_point(|func| func.entry <= off);
        let func = funcs.get(idx.checked_sub(1)?)?;
        if off < func.end { Some(func) } else { None }
    }

    #[cfg_attr(not(all(windows, target_arch = "x86_64")), allow(dead_code))]
    pub(crate) fn contains_pc(&self, runtime_pc: u64) -> bool {
        self.func_at(runtime_pc).is_some()
    }

    pub(crate) fn symbol(&self, runtime_pc: u64) -> Option<GoSymbol> {
        let func = self.func_at(runtime_pc)?;
        let name = self.tables.name(func.name_off)?;
        let off = self.text_off(runtime_pc)?;
        let (file, line) = self.tables.file_line(func, off);
        Some(GoSymbol { name, file, line })
    }
}

impl GoTables {
    fn name(&self, off: u32) -> Option<String> {
        let start = self.funcname.checked_add(off as usize)?;
        let bytes = self.data.get(start..)?;
        let end = bytes.iter().position(|byte| *byte == 0)?;
        if end == 0 || end > MAX_NAME {
            return None;
        }
        let text = std::str::from_utf8(&bytes[..end]).ok()?;
        Some(text.to_string())
    }

    fn file_line(&self, func: &FuncRec, pc_off: u64) -> (Option<String>, Option<u32>) {
        let line = self
            .pcvalue(func.pcln, u64::from(func.entry), pc_off)
            .filter(|line| *line > 0)
            .and_then(|line| u32::try_from(line).ok());
        let file = self.file_name(func, pc_off);
        (file, line)
    }

    fn file_name(&self, func: &FuncRec, pc_off: u64) -> Option<String> {
        if func.cu_off == u32::MAX {
            return None;
        }
        let index = self.pcvalue(func.pcfile, u64::from(func.entry), pc_off)?;
        if index < 0 {
            return None;
        }
        let slot = func.cu_off.checked_add(index as u32)?;
        let at = self.cutab.checked_add(slot as usize * 4)?;
        let bytes = self.data.get(at..at + 4)?;
        let name_off = u32::from_le_bytes(bytes.try_into().ok()?);
        if name_off == u32::MAX {
            return None;
        }
        let start = self.filetab.checked_add(name_off as usize)?;
        let rest = self.data.get(start..)?;
        let end = rest.iter().position(|byte| *byte == 0)?;
        if end == 0 || end > MAX_NAME {
            return None;
        }
        std::str::from_utf8(&rest[..end]).ok().map(str::to_string)
    }

    fn pcvalue(&self, off: u32, entry: u64, target: u64) -> Option<i32> {
        if off == 0 {
            return None;
        }
        let mut index = self.pctab.checked_add(off as usize)?;
        let mut val = -1i32;
        let mut pc = entry;
        let mut first = true;
        for _ in 0..100_000 {
            let (next, uv) = read_uvarint(&self.data, index)?;
            if uv == 0 && !first {
                return None;
            }
            index = next;
            val = val.checked_add(zigzag(uv))?;
            let (next, pcdelta) = read_uvarint(&self.data, index)?;
            index = next;
            let step = u64::from(pcdelta).checked_mul(u64::from(self.quantum))?;
            pc = pc.checked_add(step)?;
            first = false;
            if target < pc {
                return Some(val);
            }
        }
        None
    }
}

/// Open the pclntab in `path`, still at its link address (`bias` 0).
#[cfg_attr(not(all(windows, target_arch = "x86_64")), allow(dead_code))]
pub(crate) fn open_image(path: &Path) -> Option<GoImage> {
    let tables = read_tables(path)?;
    Some(GoImage::from_tables(tables, 0))
}

/// `runtime_text = link textStart + bias`.
///
/// Windows relocates with [`GoImage::at_base`] instead of this bias.
#[cfg_attr(windows, allow(dead_code))]
pub(crate) fn open_image_at(path: &Path, bias: u64) -> Option<GoImage> {
    let tables = read_tables(path)?;
    Some(GoImage::from_tables(tables, bias))
}

/// Preferred PE image base. The pclntab `textStart` is an address in that image.
#[cfg(all(windows, target_arch = "x86_64"))]
pub(crate) fn pe_image_base(path: &Path) -> Option<u64> {
    let mut file = File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    image_base(&mut file, len)
}

/// Leaf-first PCs. `None` when `pc` is not inside one of `images`.
///
/// `adjust_code` strips a return address (pointer authentication on macOS).
/// The leaf `pc` is already the register value the caller stripped.
pub(crate) fn unwind(
    images: &[GoImage],
    pc: u64,
    mut sp: u64,
    mut lr: u64,
    machine: GoMachine,
    mut read: impl FnMut(u64, &mut [u8]) -> bool,
    mut adjust_code: impl FnMut(u64) -> u64,
) -> Option<Vec<u64>> {
    let image = images.iter().find(|image| image.func_at(pc).is_some())?;
    let ptr = 8u64;
    let mut frames = Vec::new();
    let mut pc = pc;
    let mut innermost = true;
    while frames.len() < MAX_FRAMES {
        let Some(func) = image.func_at(pc) else {
            if walk::plausible_user_address(pc) {
                frames.push(pc);
            }
            break;
        };
        // Copy the fields before the next lookup so the func borrow ends.
        let flag = func.flag;
        let pcsp = func.pcsp;
        let entry = u64::from(func.entry);
        let top = flag & FLAG_TOP_FRAME != 0;
        let sp_write = flag & FLAG_SP_WRITE != 0;
        let pc_off = pc.wrapping_sub(image.runtime_text);
        let delta = image
            .tables
            .pcvalue(pcsp, entry, pc_off)
            .filter(|delta| *delta >= 0);
        frames.push(pc);
        if top || (!innermost && sp_write) {
            break;
        }
        let Some(delta) = delta else {
            break;
        };
        let delta = delta as u64;
        let next_sp = match machine {
            GoMachine::X86_64 => match sp.checked_add(delta).and_then(|at| at.checked_add(ptr)) {
                Some(next) => next,
                None => break,
            },
            GoMachine::Arm64 => match sp.checked_add(delta) {
                Some(next) => next,
                None => break,
            },
        };
        if next_sp < sp {
            break;
        }
        // arm64 leaves the return PC in LR until the frame stores it at SP.
        // amd64 always stores it at `SP + pcsp` (the slot the CALL pushed).
        let use_register_lr =
            machine == GoMachine::Arm64 && !((innermost && sp < next_sp) || lr == 0);
        let next_pc = if use_register_lr {
            lr
        } else {
            let ret_at = match machine {
                GoMachine::X86_64 => match sp.checked_add(delta) {
                    Some(at) => at,
                    None => break,
                },
                GoMachine::Arm64 => sp,
            };
            let mut buf = [0u8; 8];
            if !read(ret_at, &mut buf) {
                break;
            }
            adjust_code(u64::from_le_bytes(buf))
        };
        if !walk::plausible_user_address(next_pc) || (next_pc == pc && next_sp == sp) {
            break;
        }
        pc = next_pc;
        sp = next_sp;
        lr = 0;
        innermost = false;
    }
    if frames.is_empty() {
        None
    } else {
        Some(frames)
    }
}

fn read_tables(path: &Path) -> Option<GoTables> {
    let mut file = File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    if len < 64 {
        return None;
    }
    let mut magic = [0u8; 4];
    file.read_exact(&mut magic).ok()?;
    if magic == [0x7f, b'E', b'L', b'F'] {
        return elf_pclntab(&mut file, len);
    }
    let macho = u32::from_le_bytes(magic);
    let fat = u32::from_be_bytes(magic);
    if macho == 0xFEED_FACF || fat == 0xCAFE_BABE {
        return macho_pclntab(&mut file, len);
    }
    if magic[0] == b'M' && magic[1] == b'Z' {
        if !file_contains_build_info(&mut file, len) {
            return None;
        }
        return pe_pclntab(&mut file, len);
    }
    None
}

fn parse_pclntab(data: &[u8]) -> Option<GoTables> {
    if data.len() < 16 || data.len() as u64 > MAX_TABLE {
        return None;
    }
    let magic = u32::from_le_bytes(data.get(0..4)?.try_into().ok()?);
    if magic != GO_1_18 && magic != GO_1_20 {
        return None;
    }
    if data[4] != 0 || data[5] != 0 {
        return None;
    }
    let quantum = u32::from(data[6]);
    if !matches!(quantum, 1 | 2 | 4) || data[7] != 8 {
        return None;
    }
    let word = |index: usize| -> Option<u64> {
        let at = 8 + index * 8;
        Some(u64::from_le_bytes(data.get(at..at + 8)?.try_into().ok()?))
    };
    let nfunc = word(0)?;
    let text_start = word(2)?;
    let funcname = usize::try_from(word(3)?).ok()?;
    let cutab = usize::try_from(word(4)?).ok()?;
    let filetab = usize::try_from(word(5)?).ok()?;
    let pctab = usize::try_from(word(6)?).ok()?;
    let pcln = usize::try_from(word(7)?).ok()?;
    // Go 1.26 stores this word as zero and `adopt` fills it from moduledata.
    // Go 1.18 through 1.25 write the link address here, except an externally
    // linked Mach-O, whose file bytes are the addend from the `__TEXT` vmaddr.
    if nfunc == 0 || nfunc > MAX_FUNCS {
        return None;
    }
    for off in [funcname, cutab, filetab, pctab, pcln] {
        if off >= data.len() {
            return None;
        }
    }
    let functab_bytes = (nfunc as usize)
        .checked_mul(2)?
        .checked_add(1)?
        .checked_mul(4)?;
    if pcln.checked_add(functab_bytes)? > data.len() {
        return None;
    }
    let mut funcs = Vec::with_capacity(nfunc as usize);
    for index in 0..nfunc as usize {
        let at = pcln + index * 8;
        let entry = u32::from_le_bytes(data[at..at + 4].try_into().ok()?);
        let funcoff = u32::from_le_bytes(data[at + 4..at + 8].try_into().ok()?) as usize;
        let end_at = pcln + (index + 1) * 8;
        let end = u32::from_le_bytes(data[end_at..end_at + 4].try_into().ok()?);
        if end <= entry {
            return None;
        }
        let base = pcln.checked_add(funcoff)?;
        let raw = data.get(base..base + FUNC_HEAD)?;
        let entry_off = u32::from_le_bytes(raw[0..4].try_into().ok()?);
        if entry_off != entry {
            return None;
        }
        funcs.push(FuncRec {
            entry,
            end,
            name_off: u32::from_le_bytes(raw[4..8].try_into().ok()?),
            pcsp: u32::from_le_bytes(raw[16..20].try_into().ok()?),
            pcfile: u32::from_le_bytes(raw[20..24].try_into().ok()?),
            pcln: u32::from_le_bytes(raw[24..28].try_into().ok()?),
            cu_off: u32::from_le_bytes(raw[32..36].try_into().ok()?),
            flag: raw[41],
        });
    }
    let tables = GoTables {
        text_start,
        quantum,
        funcs,
        data: data.to_vec(),
        funcname,
        cutab,
        filetab,
        pctab,
    };
    // The first entry is `go:buildid` on Windows and a dotted name on ELF.
    // Either is a Go pclntab; a random byte string that merely parses is not.
    let looks_go = tables.funcs.iter().take(8).any(|func| {
        tables
            .name(func.name_off)
            .is_some_and(|name| name.contains('.') || name.starts_with("go:"))
    });
    if !looks_go {
        return None;
    }
    Some(tables)
}

/// Link address and exclusive end of the text section.
#[derive(Clone, Copy)]
struct TextSpan {
    start: u64,
    end: u64,
}

/// Link address of `runtime.text`.
///
/// A header or module value already inside the text section is that address.
/// Go 1.26’s header is zero, so the module record supplies it. An externally
/// linked Mach-O stores the Go 1.22 header as an addend from `__TEXT`’s
/// vmaddr (`segment`), either plain or as the low bits of a chained fixup.
/// The relocated address is accepted only when it lands in the section. The
/// section’s own start is not a stand-in. cgo’s `runtime.text` is not the
/// first byte there.
fn place_text(
    header: u64,
    module: Option<u64>,
    text: Option<TextSpan>,
    segment: u64,
) -> Option<u64> {
    let inside = |addr: u64| {
        text.is_some_and(|span| span.start != 0 && addr >= span.start && addr < span.end)
    };
    if inside(header) {
        return Some(header);
    }
    if let Some(addr) = module.filter(|addr| inside(*addr)) {
        return Some(addr);
    }
    if segment != 0 && header != 0 {
        for form in PtrForm::relocated() {
            if let Some(abs) = decode_ptr(header, segment, form)
                && inside(abs)
            {
                return Some(abs);
            }
        }
    }
    None
}

/// How an externally linked Mach-O stores an `R_ADDR` pointer.
///
/// `Addend` is the plain offset from `__TEXT`. `Off64` / `Vm64` are
/// `DYLD_CHAINED_PTR_64_OFFSET` and `DYLD_CHAINED_PTR_64`. `OffArm` / `VmArm`
/// are the arm64e unauthenticated rebase layouts.
#[derive(Clone, Copy)]
enum PtrForm {
    Addend,
    Off64,
    Vm64,
    OffArm,
    VmArm,
}

impl PtrForm {
    fn relocated() -> [PtrForm; 5] {
        [
            PtrForm::Addend,
            PtrForm::Off64,
            PtrForm::Vm64,
            PtrForm::OffArm,
            PtrForm::VmArm,
        ]
    }
}

fn decode_ptr(raw: u64, segment: u64, form: PtrForm) -> Option<u64> {
    if segment == 0 || raw == 0 {
        return None;
    }
    match form {
        PtrForm::Addend => (raw < segment).then_some(segment.wrapping_add(raw)),
        PtrForm::Off64 | PtrForm::Vm64 => {
            let (target, high8, next) = split_ptr64(raw)?;
            if next == 0 && high8 == 0 {
                return None;
            }
            let vm = target | (high8 << 56);
            match form {
                PtrForm::Vm64 => Some(vm),
                PtrForm::Off64 => Some(segment.wrapping_add(vm)),
                PtrForm::Addend | PtrForm::OffArm | PtrForm::VmArm => None,
            }
        }
        PtrForm::OffArm | PtrForm::VmArm => {
            let target = split_arm64e(raw)?;
            match form {
                PtrForm::VmArm => Some(target),
                PtrForm::OffArm => Some(segment.wrapping_add(target)),
                PtrForm::Addend | PtrForm::Off64 | PtrForm::Vm64 => None,
            }
        }
    }
}

/// `DYLD_CHAINED_PTR_64`: target 36, high8 8, reserved 7, next 12, bind 1.
fn split_ptr64(raw: u64) -> Option<(u64, u64, u64)> {
    if raw >> 63 != 0 || (raw >> 44) & 0x7f != 0 {
        return None;
    }
    let next = (raw >> 51) & 0xfff;
    let high8 = (raw >> 36) & 0xff;
    let target = raw & ((1 << 36) - 1);
    Some((target, high8, next))
}

/// arm64e unauthenticated rebase: target 43, next 11, bind and auth clear.
fn split_arm64e(raw: u64) -> Option<u64> {
    if raw >> 62 != 0 {
        return None;
    }
    let next = (raw >> 51) & 0x7ff;
    if next == 0 {
        return None;
    }
    Some(raw & ((1 << 43) - 1))
}

fn adopt(
    bytes: &[u8],
    module: Option<u64>,
    text: Option<TextSpan>,
    segment: u64,
) -> Option<GoTables> {
    let mut tables = parse_pclntab(bytes)?;
    tables.text_start = place_text(tables.text_start, module, text, segment)?;
    Some(tables)
}

/// `moduledata.text` for a Go 1.26 binary.
///
/// The 64-bit struct in `runtime/symtab.go` starts with `pcHeader`. `text`
/// is 176 bytes later and `etext` 8 bytes after that. An internal link, ELF,
/// and PE store those pointers as link addresses. An externally linked Mach-O
/// stores them as addends from the `__TEXT` vmaddr, plain or in a chained
/// fixup. `segment` is that vmaddr, or zero when the file already holds link
/// addresses. `pcHeader`, `text`, and `etext` have to use the same encoding.
fn module_runtime_text(
    blob: &[u8],
    pclntab_addr: u64,
    text_span: TextSpan,
    segment: u64,
) -> Option<u64> {
    if pclntab_addr == 0 || text_span.start == 0 || text_span.end <= text_span.start {
        return None;
    }
    let forms = [
        None,
        Some(PtrForm::Addend),
        Some(PtrForm::Off64),
        Some(PtrForm::Vm64),
        Some(PtrForm::OffArm),
        Some(PtrForm::VmArm),
    ];
    for form in forms {
        if form.is_some() && segment == 0 {
            continue;
        }
        if let Some(text) = module_text_hit(blob, pclntab_addr, text_span, segment, form) {
            return Some(text);
        }
    }
    None
}

fn module_text_hit(
    blob: &[u8],
    pclntab_addr: u64,
    text_span: TextSpan,
    segment: u64,
    form: Option<PtrForm>,
) -> Option<u64> {
    let mut from = 0usize;
    while from + 192 <= blob.len() {
        let raw = u64::from_le_bytes(blob.get(from..from + 8)?.try_into().ok()?);
        let pc = match form {
            None => Some(raw),
            Some(form) => decode_ptr(raw, segment, form),
        };
        if pc == Some(pclntab_addr) {
            let text_raw = u64::from_le_bytes(blob.get(from + 176..from + 184)?.try_into().ok()?);
            let etext_raw = u64::from_le_bytes(blob.get(from + 184..from + 192)?.try_into().ok()?);
            let text = match form {
                None => Some(text_raw),
                Some(form) => decode_ptr(text_raw, segment, form),
            };
            let etext = match form {
                None => Some(etext_raw),
                Some(form) => decode_ptr(etext_raw, segment, form),
            };
            if let (Some(text), Some(etext)) = (text, etext)
                && text >= text_span.start
                && text < text_span.end
                && etext > text
            {
                return Some(text);
            }
        }
        from += 8;
    }
    None
}

fn elf_pclntab<R: Read + Seek>(reader: &mut R, len: u64) -> Option<GoTables> {
    reader.seek(SeekFrom::Start(0)).ok()?;
    let mut ehdr = [0u8; 64];
    reader.read_exact(&mut ehdr).ok()?;
    if ehdr[4] != 2 || ehdr[5] != 1 {
        return None;
    }
    let shoff = u64::from_le_bytes(ehdr[40..48].try_into().ok()?);
    let shentsize = u16::from_le_bytes(ehdr[58..60].try_into().ok()?) as u64;
    let mut shnum = u16::from_le_bytes(ehdr[60..62].try_into().ok()?) as u64;
    let shstrndx = u16::from_le_bytes(ehdr[62..64].try_into().ok()?) as u64;
    if shentsize < 64 || shoff == 0 || shnum > 10_000 {
        return None;
    }
    if shnum == 0 {
        shnum = read_u64_at(reader, shoff + 4 * 8, len).unwrap_or(0);
    }
    if shstrndx >= shnum {
        return None;
    }
    let str_hdr = shoff.checked_add(shstrndx.checked_mul(shentsize)?)?;
    let str_off = read_u64_at(reader, str_hdr + 24, len)?;
    let str_size = read_u64_at(reader, str_hdr + 32, len)?;
    if str_size > MAX_TABLE {
        return None;
    }
    let mut strings = vec![0u8; str_size as usize];
    reader.seek(SeekFrom::Start(str_off)).ok()?;
    reader.read_exact(&mut strings).ok()?;
    let mut text_span = None;
    let mut pcln = None;
    let mut module = None;
    for index in 0..shnum {
        let hdr = shoff.checked_add(index.checked_mul(shentsize)?)?;
        reader.seek(SeekFrom::Start(hdr)).ok()?;
        let mut buf = [0u8; 64];
        reader.read_exact(&mut buf).ok()?;
        let name_off = u32::from_le_bytes(buf[0..4].try_into().ok()?) as usize;
        let name = cstr_at(&strings, name_off)?;
        let addr = u64::from_le_bytes(buf[16..24].try_into().ok()?);
        let off = u64::from_le_bytes(buf[24..32].try_into().ok()?);
        let size = u64::from_le_bytes(buf[32..40].try_into().ok()?);
        if name == ".text" && text_span.is_none() {
            text_span = Some(TextSpan {
                start: addr,
                end: addr.saturating_add(size),
            });
        }
        if pcln.is_none() && (name == ".gopclntab" || name == ".data.rel.ro.gopclntab") {
            pcln = Some((addr, off, size));
        }
        if name == ".go.module" && module.is_none() {
            module = Some((off, size));
        }
    }
    let (paddr, poff, psize) = pcln?;
    let bytes = read_span(reader, poff, psize, len)?;
    let magic_at = pcln_magic_at(&bytes)?;
    let pcln_addr = paddr.checked_add(magic_at as u64)?;
    let module_text = module.and_then(|(off, size)| {
        let blob = read_span(reader, off, size.min(MAX_TABLE), len)?;
        module_runtime_text(&blob, pcln_addr, text_span?, 0)
    });
    adopt(&bytes[magic_at..], module_text, text_span, 0)
}

fn macho_pclntab<R: Read + Seek>(reader: &mut R, len: u64) -> Option<GoTables> {
    let (offset, size) = macho_slice(reader, len)?;
    let layout = macho_sections(reader, offset, size)?;
    let pcln = layout.pclntab?;
    let bytes = read_span(reader, pcln.file_off, pcln.size, len)?;
    let magic_at = pcln_magic_at(&bytes)?;
    let pcln_addr = pcln.addr.checked_add(magic_at as u64)?;
    let module_text = layout.module.and_then(|module| {
        let blob = read_span(reader, module.file_off, module.size.min(MAX_TABLE), len)?;
        module_runtime_text(&blob, pcln_addr, layout.text?, layout.text_segment)
    });
    adopt(
        &bytes[magic_at..],
        module_text,
        layout.text,
        layout.text_segment,
    )
}

/// ASLR slide: runtime header address minus the link `__TEXT` vmaddr.
#[cfg(target_os = "macos")]
pub(crate) fn macho_bias(path: &Path, load_addr: u64) -> Option<u64> {
    let mut file = File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let (offset, size) = macho_slice(&mut file, len)?;
    let layout = macho_sections(&mut file, offset, size)?;
    Some(load_addr.wrapping_sub(layout.text_segment))
}

fn macho_slice<R: Read + Seek>(reader: &mut R, len: u64) -> Option<(u64, u64)> {
    reader.seek(SeekFrom::Start(0)).ok()?;
    let mut magic = [0u8; 4];
    reader.read_exact(&mut magic).ok()?;
    if u32::from_le_bytes(magic) == 0xFEED_FACF {
        return Some((0, len));
    }
    if u32::from_be_bytes(magic) != 0xCAFE_BABE {
        return None;
    }
    let mut count = [0u8; 4];
    reader.read_exact(&mut count).ok()?;
    let nfat = u32::from_be_bytes(count);
    if nfat == 0 || nfat > 64 {
        return None;
    }
    let want = macho_host_cpu()?;
    for _ in 0..nfat {
        let mut arch = [0u8; 20];
        reader.read_exact(&mut arch).ok()?;
        let cpu = u32::from_be_bytes(arch[0..4].try_into().ok()?);
        let offset = u64::from(u32::from_be_bytes(arch[8..12].try_into().ok()?));
        let size = u64::from(u32::from_be_bytes(arch[12..16].try_into().ok()?));
        if cpu == want {
            return Some((offset, size));
        }
    }
    None
}

fn macho_host_cpu() -> Option<u32> {
    #[cfg(target_arch = "aarch64")]
    {
        Some(0x0100_000C)
    }
    #[cfg(target_arch = "x86_64")]
    {
        Some(0x0100_0007)
    }
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    {
        None
    }
}

struct MachoBlob {
    addr: u64,
    file_off: u64,
    size: u64,
}

struct MachoLayout {
    /// `__TEXT` vmaddr. The ASLR slide is the runtime header minus this.
    /// An externally linked pclntab stores `textStart` as an offset from it.
    text_segment: u64,
    /// `__text` link range. Go 1.26 entry offsets are relative to `runtime.text`.
    text: Option<TextSpan>,
    /// `__gopclntab` link address and file span.
    pclntab: Option<MachoBlob>,
    /// `__go_module` file span. That is `.go.module` with Mach-O’s name rules.
    module: Option<MachoBlob>,
}

/// `__TEXT` vmaddr, the `__text` link address, and `__gopclntab`’s file span.
fn macho_sections<R: Read + Seek>(
    reader: &mut R,
    slice_off: u64,
    slice_size: u64,
) -> Option<MachoLayout> {
    reader.seek(SeekFrom::Start(slice_off)).ok()?;
    let mut header = [0u8; 32];
    reader.read_exact(&mut header).ok()?;
    if u32::from_le_bytes(header[0..4].try_into().ok()?) != 0xFEED_FACF {
        return None;
    }
    let ncmds = u32::from_le_bytes(header[16..20].try_into().ok()?) as usize;
    let sizeofcmds = u32::from_le_bytes(header[20..24].try_into().ok()?) as usize;
    if ncmds > 10_000 || sizeofcmds > 1024 * 1024 {
        return None;
    }
    let mut commands = vec![0u8; sizeofcmds];
    reader.read_exact(&mut commands).ok()?;
    let end = slice_off.saturating_add(slice_size);
    let mut text = None;
    let mut text_section = None;
    let mut section = None;
    let mut module = None;
    let mut at = 0usize;
    for _ in 0..ncmds {
        if at + 8 > commands.len() {
            break;
        }
        let cmd = u32::from_le_bytes(commands[at..at + 4].try_into().ok()?);
        let cmdsize = u32::from_le_bytes(commands[at + 4..at + 8].try_into().ok()?) as usize;
        if cmdsize < 8 || at + cmdsize > commands.len() {
            return None;
        }
        if cmd == 0x19 && cmdsize >= 72 {
            let data = &commands[at + 8..at + cmdsize];
            let name = sect_name(&data[0..16]);
            let vmaddr = u64::from_le_bytes(data[16..24].try_into().ok()?);
            if name == "__TEXT" {
                text = Some(vmaddr);
            }
            let nsects = u32::from_le_bytes(data[56..60].try_into().ok()?) as usize;
            let mut sect = 64usize;
            for _ in 0..nsects {
                if sect + 80 > data.len() {
                    break;
                }
                let raw = &data[sect..sect + 80];
                let sect_name = sect_name(&raw[0..16]);
                let addr = u64::from_le_bytes(raw[32..40].try_into().ok()?);
                let size = u64::from_le_bytes(raw[40..48].try_into().ok()?);
                let file_off = u64::from(u32::from_le_bytes(raw[48..52].try_into().ok()?));
                let abs = slice_off.checked_add(file_off);
                let in_slice = abs
                    .and_then(|off| off.checked_add(size))
                    .is_some_and(|last| last <= end);
                if sect_name == "__text" && text_section.is_none() {
                    text_section = Some(TextSpan {
                        start: addr,
                        end: addr.saturating_add(size),
                    });
                }
                if sect_name == "__gopclntab" && in_slice {
                    section = Some(MachoBlob {
                        addr,
                        file_off: abs?,
                        size,
                    });
                }
                if sect_name == "__go_module" && module.is_none() && in_slice {
                    module = Some(MachoBlob {
                        addr,
                        file_off: abs?,
                        size,
                    });
                }
                sect += 80;
            }
        }
        at += cmdsize;
    }
    Some(MachoLayout {
        text_segment: text?,
        text: text_section,
        pclntab: section,
        module,
    })
}

fn pe_pclntab<R: Read + Seek>(reader: &mut R, len: u64) -> Option<GoTables> {
    let sections = pe_sections(reader, len)?;
    let base = image_base(reader, len).unwrap_or(0);
    let text = sections
        .iter()
        .find(|section| section.name == ".text")
        .map(|section| {
            let start = base.wrapping_add(section.virtual_addr);
            let size = if section.virtual_size == 0 {
                section.raw_size
            } else {
                section.virtual_size
            };
            TextSpan {
                start,
                end: start.saturating_add(size),
            }
        });
    for name in [".gopclntab", ".rdata", ".data"] {
        let Some(section) = sections.iter().find(|section| section.name == name) else {
            continue;
        };
        let Some(bytes) = read_span(
            reader,
            section.raw_ptr,
            section.raw_size.min(MAX_TABLE),
            len,
        ) else {
            continue;
        };
        let Some(magic_at) = pcln_magic_at(&bytes) else {
            continue;
        };
        let pcln_addr = base
            .wrapping_add(section.virtual_addr)
            .wrapping_add(magic_at as u64);
        let module_text = sections.iter().find_map(|candidate| {
            if candidate.name != ".go.module" && candidate.name != ".data" {
                return None;
            }
            let blob = read_span(
                reader,
                candidate.raw_ptr,
                candidate.raw_size.min(MAX_TABLE),
                len,
            )?;
            module_runtime_text(&blob, pcln_addr, text?, 0)
        });
        if let Some(tables) = adopt(&bytes[magic_at..], module_text, text, 0) {
            return Some(tables);
        }
    }
    None
}

fn pcln_magic_at(bytes: &[u8]) -> Option<usize> {
    let magic = [0xf1, 0xff, 0xff, 0xff];
    let older = [0xf0, 0xff, 0xff, 0xff];
    let mut at = 0usize;
    while at + 16 < bytes.len() {
        if (bytes[at..at + 4] == magic || bytes[at..at + 4] == older)
            && parse_pclntab(&bytes[at..]).is_some()
        {
            return Some(at);
        }
        at += 4;
    }
    None
}

fn file_contains_build_info<R: Read + Seek>(reader: &mut R, len: u64) -> bool {
    let cap = len.min(MAX_TABLE);
    reader.seek(SeekFrom::Start(0)).ok();
    let mut pending = Vec::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut scanned = 0u64;
    while scanned < cap {
        let n = reader.read(&mut buf).unwrap_or(0);
        if n == 0 {
            break;
        }
        pending.extend_from_slice(&buf[..n]);
        if pending
            .windows(BUILD_INFO.len())
            .any(|window| window == BUILD_INFO)
        {
            return true;
        }
        let keep = BUILD_INFO.len() - 1;
        if pending.len() > keep {
            pending.drain(..pending.len() - keep);
        }
        scanned += n as u64;
    }
    false
}

struct PeSection {
    name: String,
    virtual_size: u64,
    virtual_addr: u64,
    raw_size: u64,
    raw_ptr: u64,
}

fn pe_sections<R: Read + Seek>(reader: &mut R, len: u64) -> Option<Vec<PeSection>> {
    reader.seek(SeekFrom::Start(0x3c)).ok()?;
    let mut lfanew_buf = [0u8; 4];
    reader.read_exact(&mut lfanew_buf).ok()?;
    let lfanew = u32::from_le_bytes(lfanew_buf) as u64;
    if lfanew > 1024 * 1024 || lfanew + 24 > len {
        return None;
    }
    reader.seek(SeekFrom::Start(lfanew)).ok()?;
    let mut pe = [0u8; 24];
    reader.read_exact(&mut pe).ok()?;
    if pe[0..4] != *b"PE\0\0" {
        return None;
    }
    let nsections = u16::from_le_bytes(pe[6..8].try_into().ok()?) as u64;
    let opt_size = u16::from_le_bytes(pe[20..22].try_into().ok()?) as u64;
    if nsections > 96 || opt_size > 1024 * 1024 {
        return None;
    }
    let start = lfanew.checked_add(24)?.checked_add(opt_size)?;
    let mut sections = Vec::new();
    for index in 0..nsections {
        let at = start.checked_add(index.checked_mul(40)?)?;
        reader.seek(SeekFrom::Start(at)).ok()?;
        let mut raw = [0u8; 40];
        reader.read_exact(&mut raw).ok()?;
        let name_end = raw[..8].iter().position(|byte| *byte == 0).unwrap_or(8);
        let name = String::from_utf8_lossy(&raw[..name_end]).into_owned();
        let virtual_size = u32::from_le_bytes(raw[8..12].try_into().ok()?) as u64;
        let virtual_addr = u32::from_le_bytes(raw[12..16].try_into().ok()?) as u64;
        let raw_size = u32::from_le_bytes(raw[16..20].try_into().ok()?) as u64;
        let raw_ptr = u32::from_le_bytes(raw[20..24].try_into().ok()?) as u64;
        sections.push(PeSection {
            name,
            virtual_size,
            virtual_addr,
            raw_size,
            raw_ptr,
        });
    }
    Some(sections)
}

fn image_base<R: Read + Seek>(reader: &mut R, len: u64) -> Option<u64> {
    reader.seek(SeekFrom::Start(0x3c)).ok()?;
    let mut lfanew_buf = [0u8; 4];
    reader.read_exact(&mut lfanew_buf).ok()?;
    let lfanew = u32::from_le_bytes(lfanew_buf) as u64;
    reader.seek(SeekFrom::Start(lfanew)).ok()?;
    let mut pe = [0u8; 24];
    reader.read_exact(&mut pe).ok()?;
    if pe[0..4] != *b"PE\0\0" {
        return None;
    }
    let opt_size = u16::from_le_bytes(pe[20..22].try_into().ok()?) as u64;
    if opt_size < 32 || lfanew + 24 + 32 > len {
        return None;
    }
    reader.seek(SeekFrom::Start(lfanew + 24)).ok()?;
    let mut magic = [0u8; 2];
    reader.read_exact(&mut magic).ok()?;
    if u16::from_le_bytes(magic) != 0x20B {
        return None;
    }
    // PE32+ ImageBase is 24 bytes into the optional header.
    reader.seek(SeekFrom::Start(lfanew + 24 + 24)).ok()?;
    let mut base = [0u8; 8];
    reader.read_exact(&mut base).ok()?;
    Some(u64::from_le_bytes(base))
}

fn read_span<R: Read + Seek>(reader: &mut R, off: u64, size: u64, len: u64) -> Option<Vec<u8>> {
    if !(16..=MAX_TABLE).contains(&size) || off.checked_add(size)? > len {
        return None;
    }
    reader.seek(SeekFrom::Start(off)).ok()?;
    let mut buf = vec![0u8; size as usize];
    reader.read_exact(&mut buf).ok()?;
    Some(buf)
}

fn read_u64_at<R: Read + Seek>(reader: &mut R, at: u64, len: u64) -> Option<u64> {
    if at.checked_add(8)? > len {
        return None;
    }
    reader.seek(SeekFrom::Start(at)).ok()?;
    let mut buf = [0u8; 8];
    reader.read_exact(&mut buf).ok()?;
    Some(u64::from_le_bytes(buf))
}

fn cstr_at(bytes: &[u8], off: usize) -> Option<&str> {
    let rest = bytes.get(off..)?;
    let end = rest
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(rest.len());
    std::str::from_utf8(&rest[..end]).ok()
}

fn sect_name(bytes: &[u8]) -> &str {
    let end = bytes
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(bytes.len());
    std::str::from_utf8(&bytes[..end]).unwrap_or("")
}

fn read_uvarint(data: &[u8], mut index: usize) -> Option<(usize, u32)> {
    let mut value = 0u32;
    let mut shift = 0u32;
    for _ in 0..5 {
        let byte = *data.get(index)?;
        index += 1;
        value |= u32::from(byte & 0x7f).checked_shl(shift)?;
        if byte & 0x80 == 0 {
            return Some((index, value));
        }
        shift += 7;
    }
    None
}

fn zigzag(value: u32) -> i32 {
    let magnitude = value >> 1;
    if value & 1 == 0 {
        magnitude as i32
    } else {
        (!magnitude) as i32
    }
}

#[cfg(test)]
mod tests {
    use super::{
        BUILD_INFO, GoImage, GoMachine, file_contains_build_info, image_base, open_image,
        open_image_at, parse_pclntab, unwind,
    };
    use std::collections::HashMap;

    #[test]
    fn a_synthetic_table_names_the_leaf_and_walks_pcsp() {
        let bytes = synthetic();
        let tables = parse_pclntab(&bytes).expect("pclntab");
        let image = GoImage::from_tables(tables, 0x1000);
        let spin = image.symbol(0x401110).expect("spin");
        assert_eq!(spin.name, "main.spin");
        assert_eq!(spin.file.as_deref(), Some("a.go"));
        assert_eq!(spin.line, Some(10));
        let caller = image.symbol(0x401190).expect("caller");
        assert_eq!(caller.name, "main.caller");
        assert_eq!(caller.line, Some(20));

        let sp = 0x7fff_0000_1000u64;
        let mut memory = HashMap::new();
        // spin's pcsp is 0, so the return PC is at SP. 0x401190 is 16 bytes
        // into main.caller (entry 0x401180), the same split gentraceback uses:
        // the walk keeps the raw return PC and symbol lookup names that PC.
        memory.insert(sp, 0x401190u64);
        memory.insert(sp + 16, 0x5555_0000_2000u64);
        let frames = unwind(
            &[image],
            0x401110,
            sp,
            0,
            GoMachine::X86_64,
            |addr, buf| {
                let Some(value) = memory.get(&addr) else {
                    return false;
                };
                buf[..8].copy_from_slice(&value.to_le_bytes());
                true
            },
            |addr| addr,
        )
        .expect("walk");
        assert_eq!(frames, vec![0x401110, 0x401190, 0x5555_0000_2000]);
    }

    #[test]
    fn arm64_keeps_the_link_register_until_the_frame_saves_it() {
        let image = GoImage::from_tables(parse_pclntab(&synthetic()).unwrap(), 0x1000);
        let sp = 0x7fff_0000_1000u64;
        let mut memory = HashMap::new();
        // The leaf's pcsp is 0, so LR is still the caller. The caller's pcsp
        // is 8 and its return PC is the word at SP, not SP+pcsp+8.
        memory.insert(sp, 0x5555_0000_2000u64);
        let frames = unwind(
            &[image],
            0x401110,
            sp,
            0x401190,
            GoMachine::Arm64,
            |addr, buf| {
                let Some(value) = memory.get(&addr) else {
                    return false;
                };
                buf[..8].copy_from_slice(&value.to_le_bytes());
                true
            },
            |addr| addr,
        )
        .expect("arm64 walk");
        assert_eq!(frames, vec![0x401110, 0x401190, 0x5555_0000_2000]);
    }

    #[test]
    fn a_pc_outside_the_module_is_not_a_go_walk() {
        let image = GoImage::from_tables(parse_pclntab(&synthetic()).unwrap(), 0);
        assert!(
            unwind(
                &[image],
                0x5555_0000_1000,
                0x7fff_0000_1000,
                0,
                GoMachine::X86_64,
                |_, _| false,
                |addr| addr,
            )
            .is_none()
        );
    }

    #[test]
    fn build_info_magic_must_include_the_marker_byte() {
        let mut cursor = std::io::Cursor::new(BUILD_INFO.to_vec());
        assert!(file_contains_build_info(
            &mut cursor,
            BUILD_INFO.len() as u64
        ));
        let mut plain = std::io::Cursor::new(b"Go buildinf:".to_vec());
        assert!(!file_contains_build_info(
            &mut plain,
            b"Go buildinf:".len() as u64
        ));
        let mut nospace = std::io::Cursor::new(b"\xffGo buildinf:".to_vec());
        assert!(!file_contains_build_info(
            &mut nospace,
            b"\xffGo buildinf:".len() as u64
        ));
    }

    #[test]
    fn a_real_go_toolchain_pclntab_names_main_spin() {
        let go = match which_go() {
            Some(go) => go,
            None if required() => panic!("CI requires a Go toolchain"),
            None => return,
        };
        let dir = std::env::temp_dir().join(format!(
            "anyprof-go-pcln-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let source = dir.join("main.go");
        std::fs::write(
            &source,
            r#"package main
func main() { caller() }
//go:noinline
func caller() { spin() }
//go:noinline
func spin() {
    var x uint64
    for { x++ }
}
"#,
        )
        .unwrap();
        let binary = dir.join(if cfg!(windows) { "spin.exe" } else { "spin" });
        let compile = std::process::Command::new(&go)
            .args(["build", "-ldflags=-s -w", "-o"])
            .arg(&binary)
            .arg(&source)
            .env("CGO_ENABLED", "0")
            .current_dir(&dir)
            .output()
            .unwrap_or_else(|err| panic!("spawn {go}: {err}"));
        assert!(
            compile.status.success(),
            "go build\n{}",
            String::from_utf8_lossy(&compile.stderr)
        );
        let image = open_image(&binary).expect("pclntab in a stripped Go binary");
        let found = find_named(&image, "main.spin");
        assert!(
            found
                .file
                .as_deref()
                .is_some_and(|file| file.ends_with("main.go")),
            "file {:?}",
            found.file
        );
        assert!(found.line.is_some_and(|line| line > 0), "{found:?}");
        let caller = find_named(&image, "main.caller");
        assert_eq!(caller.file, found.file);
        // The same pclntab layout is what macOS and Windows will open. Cross
        // compile proves the Mach-O and PE readers, including a relocated PE
        // image base, without those hosts.
        for (goos, goarch) in [
            ("windows", "amd64"),
            ("darwin", "amd64"),
            ("darwin", "arm64"),
        ] {
            let binary = dir.join(format!("spin-{goos}-{goarch}"));
            let compile = std::process::Command::new(&go)
                .args(["build", "-ldflags=-s -w", "-o"])
                .arg(&binary)
                .arg(&source)
                .env("CGO_ENABLED", "0")
                .env("GOOS", goos)
                .env("GOARCH", goarch)
                .current_dir(&dir)
                .output()
                .unwrap_or_else(|err| panic!("spawn {go}: {err}"));
            assert!(
                compile.status.success(),
                "go build {goos}/{goarch}\n{}",
                String::from_utf8_lossy(&compile.stderr)
            );
            let image = open_image(&binary)
                .unwrap_or_else(|| panic!("pclntab in the stripped {goos}/{goarch} binary"));
            let found = find_named(&image, "main.spin");
            assert!(
                found
                    .file
                    .as_deref()
                    .is_some_and(|file| file.ends_with("main.go")),
                "{goos}/{goarch} file {:?}",
                found.file
            );
            if goos == "windows" {
                let mut file = std::fs::File::open(&binary).unwrap();
                let len = file.metadata().unwrap().len();
                let preferred = image_base(&mut file, len).expect("PE32+ image base");
                assert!(
                    image.runtime_text > preferred,
                    "textStart {:#x} should sit above the image base {preferred:#x}",
                    image.runtime_text
                );
                let slide = 0x1_0000_0000u64;
                let slid = open_image_at(&binary, slide).unwrap();
                let moved = slid.symbol(find_pc(&image, "main.spin").wrapping_add(slide));
                assert_eq!(
                    moved.as_ref().map(|symbol| symbol.name.as_str()),
                    Some("main.spin")
                );
            }
            // The stripped scan above still names a function if runtime_text
            // is wrong by a constant. The linker's runtime.text is the check.
            let linked = dir.join(format!("linked-{goos}-{goarch}"));
            go_build(&go, &source, &linked, &dir, Some((goos, goarch)), false);
            assert_link_text(&go, &linked);
        }
        let linked = dir.join(if cfg!(windows) {
            "linked.exe"
        } else {
            "linked"
        });
        go_build(&go, &source, &linked, &dir, None, false);
        assert_link_text(&go, &linked);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// cgo's `runtime.text` sits past the first byte of the text section.
    /// Using that section address shifts every entry. `go tool nm` is the
    /// link address the pclntab offsets are relative to.
    #[test]
    fn a_cgo_binary_uses_moduledata_text() {
        let go = match which_go() {
            Some(go) => go,
            None if required() => panic!("CI requires a Go toolchain"),
            None => return,
        };
        if !c_compiler_runs() {
            if required() && cfg!(unix) {
                panic!("cgo text check needs a C compiler");
            }
            return;
        }
        let dir = temp_module("anyprof-go-cgo");
        let source = r#"package main
/*
static volatile int sink;
void touch(void) { sink++; }
*/
import "C"
func main() { caller() }
//go:noinline
func caller() { C.touch(); spin() }
//go:noinline
func spin() {
    var x uint64
    for { x++ }
}
"#;
        std::fs::write(dir.join("main.go"), source).unwrap();
        let init = std::process::Command::new(&go)
            .args(["mod", "init", "anyprof.cgo"])
            .current_dir(&dir)
            .env("GO111MODULE", "on")
            .env("CGO_ENABLED", "1")
            .output()
            .unwrap_or_else(|err| panic!("spawn {go}: {err}"));
        assert!(
            init.status.success(),
            "go mod init\n{}",
            String::from_utf8_lossy(&init.stderr)
        );
        let binary = dir.join(if cfg!(windows) { "cgo.exe" } else { "cgo" });
        let build = std::process::Command::new(&go)
            .args(["build", "-o"])
            .arg(&binary)
            .arg(".")
            .current_dir(&dir)
            .env("GO111MODULE", "on")
            .env("CGO_ENABLED", "1")
            .output()
            .unwrap_or_else(|err| panic!("spawn {go}: {err}"));
        if !build.status.success() {
            let err = String::from_utf8_lossy(&build.stderr);
            if required() && cfg!(unix) {
                panic!("cgo build failed\n{err}");
            }
            eprintln!("skipping cgo text check: {err}");
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
        assert_link_text(&go, &binary);
        let stripped = dir.join(if cfg!(windows) {
            "cgo-stripped.exe"
        } else {
            "cgo-stripped"
        });
        let strip = std::process::Command::new(&go)
            .args(["build", "-ldflags=-s -w", "-o"])
            .arg(&stripped)
            .arg(".")
            .current_dir(&dir)
            .env("GO111MODULE", "on")
            .env("CGO_ENABLED", "1")
            .output()
            .unwrap_or_else(|err| panic!("spawn {go}: {err}"));
        assert!(
            strip.status.success(),
            "stripped cgo build\n{}",
            String::from_utf8_lossy(&strip.stderr)
        );
        // Two cgo links do not share addresses. The stripped file has to
        // open, and its text base has to name main.spin. The nm check above
        // is what rejects a base of `.text` when runtime.text is further in.
        let image = open_image(&stripped).expect("pclntab in a stripped cgo binary");
        let _ = find_named(&image, "main.spin");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn temp_module(prefix: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "{prefix}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        #[cfg(unix)]
        {
            std::fs::canonicalize(&dir).unwrap_or(dir)
        }
        #[cfg(not(unix))]
        {
            dir
        }
    }

    fn c_compiler_runs() -> bool {
        for compiler in ["cc", "gcc", "clang"] {
            if std::process::Command::new(compiler)
                .arg("--version")
                .output()
                .is_ok_and(|output| output.status.success())
            {
                return true;
            }
        }
        false
    }

    fn go_build(
        go: &str,
        source: &std::path::Path,
        binary: &std::path::Path,
        dir: &std::path::Path,
        target: Option<(&str, &str)>,
        strip: bool,
    ) {
        let mut command = std::process::Command::new(go);
        command.arg("build");
        if strip {
            command.arg("-ldflags=-s -w");
        }
        command
            .arg("-o")
            .arg(binary)
            .arg(source)
            .env("CGO_ENABLED", "0")
            .current_dir(dir);
        if let Some((goos, goarch)) = target {
            command.env("GOOS", goos).env("GOARCH", goarch);
        }
        let compile = command
            .output()
            .unwrap_or_else(|err| panic!("spawn {go}: {err}"));
        let label = target
            .map(|(goos, goarch)| format!("{goos}/{goarch}"))
            .unwrap_or_else(|| "host".to_string());
        assert!(
            compile.status.success(),
            "go build {label}\n{}",
            String::from_utf8_lossy(&compile.stderr)
        );
    }

    fn assert_link_text(go: &str, binary: &std::path::Path) {
        let image = open_image(binary).unwrap_or_else(|| panic!("pclntab in {}", binary.display()));
        let runtime_text = nm_symbol(go, binary, "runtime.text");
        assert_eq!(
            image.runtime_text,
            runtime_text,
            "runtime.text in {}",
            binary.display()
        );
        let spin = nm_symbol(go, binary, "main.spin");
        let symbol = image.symbol(spin).unwrap_or_else(|| {
            panic!(
                "main.spin at {spin:#x} with runtime.text {:#x}",
                image.runtime_text
            )
        });
        assert_eq!(symbol.name, "main.spin");
        assert!(
            symbol
                .file
                .as_deref()
                .is_some_and(|file| file.ends_with("main.go")),
            "file {:?}",
            symbol.file
        );
        assert!(symbol.line.is_some_and(|line| line > 0), "{symbol:?}");
    }

    fn nm_symbol(go: &str, binary: &std::path::Path, name: &str) -> u64 {
        let output = std::process::Command::new(go)
            .args(["tool", "nm"])
            .arg(binary)
            .output()
            .unwrap_or_else(|err| panic!("spawn {go} tool nm: {err}"));
        assert!(
            output.status.success(),
            "go tool nm {}\n{}",
            binary.display(),
            String::from_utf8_lossy(&output.stderr)
        );
        let text = String::from_utf8_lossy(&output.stdout);
        for line in text.lines() {
            let mut parts = line.split_whitespace();
            let Some(addr) = parts.next() else { continue };
            if parts.next_back() == Some(name) {
                return u64::from_str_radix(addr, 16)
                    .unwrap_or_else(|err| panic!("nm address {addr}: {err}"));
            }
        }
        panic!("go tool nm did not list {name} in {}", binary.display());
    }

    fn find_pc(image: &GoImage, name: &str) -> u64 {
        let limit = image
            .tables
            .funcs
            .last()
            .map(|func| u64::from(func.end))
            .unwrap_or(0);
        for off in (0..limit).step_by(4) {
            let pc = image.runtime_text.wrapping_add(off);
            if image.symbol(pc).is_some_and(|symbol| symbol.name == name) {
                return pc;
            }
        }
        panic!("pclntab did not contain {name}");
    }

    fn find_named(image: &GoImage, name: &str) -> super::GoSymbol {
        let limit = image
            .tables
            .funcs
            .last()
            .map(|func| u64::from(func.end))
            .unwrap_or(0);
        for off in (0..limit).step_by(4) {
            let pc = image.runtime_text.wrapping_add(off);
            if let Some(symbol) = image.symbol(pc)
                && symbol.name == name
            {
                return symbol;
            }
        }
        panic!("pclntab did not contain {name} within text of {limit:#x} bytes");
    }

    fn which_go() -> Option<String> {
        let output = std::process::Command::new(if cfg!(windows) { "where" } else { "which" })
            .arg("go")
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let text = String::from_utf8(output.stdout).ok()?;
        let path = text.lines().next()?.trim();
        if path.is_empty() {
            None
        } else {
            Some(path.to_string())
        }
    }

    fn required() -> bool {
        std::env::var("ANYPROF_REQUIRE_NATIVE").ok().as_deref() == Some("1")
    }

    /// Externally linked Mach-O stores Go 1.22 `textStart` as an addend from
    /// the `__TEXT` vmaddr. Internal link and Go 1.26 already store the link
    /// address, and a cgo base that misses `__text` is not replaced with the
    /// section start.
    #[test]
    fn macho_text_start_follows_the_link() {
        const TEXT_VM: u64 = 0x1_0000_0000;
        const TEXT_ADDR: u64 = 0x1_0000_1000;
        const TEXT_SIZE: u64 = 0x1_0000;
        const PCLN_ADDR: u64 = 0x1_0020_0000;
        let dir = temp_module("anyprof-macho-text");

        let mut external = synthetic();
        put_u64(&mut external, 24, 0x1AA0);
        let image = open_macho(
            &dir,
            "external",
            &macho_image(TEXT_VM, TEXT_ADDR, TEXT_SIZE, PCLN_ADDR, &external, None),
        );
        assert_eq!(image.runtime_text, TEXT_VM + 0x1AA0);
        assert_eq!(
            image
                .symbol(image.runtime_text + 0x110)
                .as_ref()
                .map(|symbol| symbol.name.as_str()),
            Some("main.spin")
        );

        let mut internal = synthetic();
        put_u64(&mut internal, 24, TEXT_ADDR);
        let image = open_macho(
            &dir,
            "internal-arm64",
            &macho_image(TEXT_VM, TEXT_ADDR, TEXT_SIZE, PCLN_ADDR, &internal, None),
        );
        assert_eq!(image.runtime_text, TEXT_ADDR);

        let mut amd64 = synthetic();
        put_u64(&mut amd64, 24, 0x10_01000);
        let image = open_macho(
            &dir,
            "internal-amd64",
            &macho_image(0x100_0000, 0x10_01000, TEXT_SIZE, 0x20_00000, &amd64, None),
        );
        assert_eq!(image.runtime_text, 0x10_01000);

        let mut zero = synthetic();
        put_u64(&mut zero, 24, 0);
        let missing = dir.join("zero");
        std::fs::write(
            &missing,
            macho_image(TEXT_VM, TEXT_ADDR, TEXT_SIZE, PCLN_ADDR, &zero, None),
        )
        .unwrap();
        assert!(open_image(&missing).is_none());

        let mut miss = synthetic();
        put_u64(&mut miss, 24, 0x20);
        let missing = dir.join("miss");
        std::fs::write(
            &missing,
            macho_image(TEXT_VM, TEXT_ADDR, TEXT_SIZE, PCLN_ADDR, &miss, None),
        )
        .unwrap();
        assert!(open_image(&missing).is_none());

        let module_text = TEXT_ADDR + 0x100;
        let image = open_macho(
            &dir,
            "module",
            &macho_image(
                TEXT_VM,
                TEXT_ADDR,
                TEXT_SIZE,
                PCLN_ADDR,
                &zero,
                Some(module_record(PCLN_ADDR, module_text)),
            ),
        );
        assert_eq!(image.runtime_text, module_text);

        let image = open_macho(
            &dir,
            "module-both-addends",
            &macho_image(
                TEXT_VM,
                TEXT_ADDR,
                TEXT_SIZE,
                PCLN_ADDR,
                &zero,
                Some(module_record(PCLN_ADDR - TEXT_VM, module_text - TEXT_VM)),
            ),
        );
        assert_eq!(image.runtime_text, module_text);

        let chained = |offset: u64, next: u64| offset | (next << 51);
        let mut chain_header = synthetic();
        put_u64(&mut chain_header, 24, chained(0x1AA0, 4));
        let image = open_macho(
            &dir,
            "chained-header",
            &macho_image(
                TEXT_VM,
                TEXT_ADDR,
                TEXT_SIZE,
                PCLN_ADDR,
                &chain_header,
                None,
            ),
        );
        assert_eq!(image.runtime_text, TEXT_VM + 0x1AA0);

        let mut chained_module = module_record(0, 0);
        chained_module[..8].copy_from_slice(&chained(PCLN_ADDR - TEXT_VM, 2).to_le_bytes());
        chained_module[176..184].copy_from_slice(&chained(module_text - TEXT_VM, 2).to_le_bytes());
        chained_module[184..192]
            .copy_from_slice(&chained(module_text - TEXT_VM + 0x100, 1).to_le_bytes());
        let image = open_macho(
            &dir,
            "chained-module",
            &macho_image(
                TEXT_VM,
                TEXT_ADDR,
                TEXT_SIZE,
                PCLN_ADDR,
                &zero,
                Some(chained_module),
            ),
        );
        assert_eq!(image.runtime_text, module_text);
    }

    fn open_macho(dir: &std::path::Path, name: &str, bytes: &[u8]) -> GoImage {
        let path = dir.join(name);
        std::fs::write(&path, bytes).unwrap();
        open_image(&path).unwrap_or_else(|| panic!("pclntab in {name}"))
    }

    fn module_record(pcln_addr: u64, text: u64) -> Vec<u8> {
        let mut blob = vec![0u8; 192];
        blob[..8].copy_from_slice(&pcln_addr.to_le_bytes());
        blob[176..184].copy_from_slice(&text.to_le_bytes());
        blob[184..192].copy_from_slice(&(text + 0x100).to_le_bytes());
        blob
    }

    fn macho_image(
        text_vm: u64,
        text_addr: u64,
        text_size: u64,
        pcln_addr: u64,
        pcln: &[u8],
        module: Option<Vec<u8>>,
    ) -> Vec<u8> {
        let nsects = if module.is_some() { 3u32 } else { 2 };
        let cmdsize = 8 + 64 + 80 * nsects;
        let mut file = Vec::new();
        for word in [0xFEED_FACF, 0x0100_0007, 0, 2, 1, cmdsize, 0, 0] {
            file.extend_from_slice(&word.to_le_bytes());
        }
        file.extend_from_slice(&0x19u32.to_le_bytes());
        file.extend_from_slice(&cmdsize.to_le_bytes());
        macho_name(&mut file, b"__TEXT");
        file.extend_from_slice(&text_vm.to_le_bytes());
        file.extend_from_slice(&(text_size + 0x1000).to_le_bytes());
        file.extend_from_slice(&0u64.to_le_bytes());
        file.extend_from_slice(&0u64.to_le_bytes());
        file.extend_from_slice(&7u32.to_le_bytes());
        file.extend_from_slice(&5u32.to_le_bytes());
        file.extend_from_slice(&nsects.to_le_bytes());
        file.extend_from_slice(&0u32.to_le_bytes());
        let content_off = file.len() + 80 * nsects as usize;
        let pcln_off = content_off as u32;
        macho_section(&mut file, b"__text", text_addr, text_size, 0);
        macho_section(
            &mut file,
            b"__gopclntab",
            pcln_addr,
            pcln.len() as u64,
            pcln_off,
        );
        if let Some(bytes) = &module {
            macho_section(
                &mut file,
                b"__go_module",
                pcln_addr + 0x10000,
                bytes.len() as u64,
                pcln_off + pcln.len() as u32,
            );
        }
        assert_eq!(file.len(), content_off);
        file.extend_from_slice(pcln);
        if let Some(bytes) = module {
            file.extend_from_slice(&bytes);
        }
        file
    }

    fn macho_section(out: &mut Vec<u8>, name: &[u8], addr: u64, size: u64, file_off: u32) {
        macho_name(out, name);
        macho_name(out, b"__TEXT");
        out.extend_from_slice(&addr.to_le_bytes());
        out.extend_from_slice(&size.to_le_bytes());
        out.extend_from_slice(&file_off.to_le_bytes());
        for _ in 0..7 {
            out.extend_from_slice(&0u32.to_le_bytes());
        }
    }

    fn macho_name(out: &mut Vec<u8>, name: &[u8]) {
        let mut bytes = [0u8; 16];
        bytes[..name.len()].copy_from_slice(name);
        out.extend_from_slice(&bytes);
    }

    fn synthetic() -> Vec<u8> {
        let names = b"main.spin\0main.caller\0";
        let files = b"a.go\0";
        let cutab = 0u32.to_le_bytes();
        let spin_pcsp = pcdata(0);
        let caller_pcsp = pcdata(8);
        let file_idx = pcdata(0);
        let spin_line = pcdata(10);
        let caller_line = pcdata(20);
        // Offset 0 in a pcdata field means "no table", so the streams start at 1.
        let mut pctab = vec![0u8];
        let spin_pcsp_off = push_stream(&mut pctab, &spin_pcsp);
        let caller_pcsp_off = push_stream(&mut pctab, &caller_pcsp);
        let file_off = push_stream(&mut pctab, &file_idx);
        let spin_line_off = push_stream(&mut pctab, &spin_line);
        let caller_line_off = push_stream(&mut pctab, &caller_line);

        let mut body = Vec::new();
        body.extend_from_slice(names);
        let file_at = body.len();
        body.extend_from_slice(files);
        let cu_at = body.len();
        body.extend_from_slice(&cutab);
        let pctab_at = body.len();
        body.extend_from_slice(&pctab);
        let pcln_at = body.len();
        // functab: two funcs plus a sentinel entry. funcoff is relative to pcln.
        let func0 = 20u32;
        let func1 = 64u32;
        body.extend_from_slice(&0x100u32.to_le_bytes());
        body.extend_from_slice(&func0.to_le_bytes());
        body.extend_from_slice(&0x180u32.to_le_bytes());
        body.extend_from_slice(&func1.to_le_bytes());
        body.extend_from_slice(&0x200u32.to_le_bytes());
        write_func(
            &mut body,
            0x100,
            0,
            spin_pcsp_off,
            file_off,
            spin_line_off,
            0,
        );
        write_func(
            &mut body,
            0x180,
            10,
            caller_pcsp_off,
            file_off,
            caller_line_off,
            0,
        );

        let mut data = vec![0u8; 72];
        data[0..4].copy_from_slice(&0xFFFF_FFF1u32.to_le_bytes());
        data[6] = 1;
        data[7] = 8;
        put_u64(&mut data, 8, 2);
        put_u64(&mut data, 16, 1);
        put_u64(&mut data, 24, 0x400000);
        put_u64(&mut data, 32, 72);
        put_u64(&mut data, 40, 72 + cu_at as u64);
        put_u64(&mut data, 48, 72 + file_at as u64);
        put_u64(&mut data, 56, 72 + pctab_at as u64);
        put_u64(&mut data, 64, 72 + pcln_at as u64);
        data.extend_from_slice(&body);
        let _ = file_at;
        data
    }

    fn push_stream(out: &mut Vec<u8>, bytes: &[u8]) -> u32 {
        let off = out.len() as u32;
        out.extend_from_slice(bytes);
        off
    }

    fn pcdata(value: i32) -> Vec<u8> {
        let mut out = Vec::new();
        write_uvarint(&mut out, zigzag_encode(value + 1));
        write_uvarint(&mut out, 0x10000);
        out.push(0);
        out
    }

    fn zigzag_encode(delta: i32) -> u32 {
        if delta >= 0 {
            (delta as u32) << 1
        } else {
            (((-delta - 1) as u32) << 1) | 1
        }
    }

    fn write_uvarint(out: &mut Vec<u8>, mut value: u32) {
        loop {
            let mut byte = (value & 0x7f) as u8;
            value >>= 7;
            if value != 0 {
                byte |= 0x80;
            }
            out.push(byte);
            if value == 0 {
                break;
            }
        }
    }

    fn write_func(
        out: &mut Vec<u8>,
        entry: u32,
        name_off: u32,
        pcsp: u32,
        pcfile: u32,
        pcln: u32,
        cu: u32,
    ) {
        let start = out.len();
        out.extend_from_slice(&entry.to_le_bytes());
        out.extend_from_slice(&name_off.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&pcsp.to_le_bytes());
        out.extend_from_slice(&pcfile.to_le_bytes());
        out.extend_from_slice(&pcln.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&cu.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&[0, 0, 0, 0]);
        assert_eq!(out.len() - start, 44);
    }

    fn put_u64(data: &mut [u8], at: usize, value: u64) {
        data[at..at + 8].copy_from_slice(&value.to_le_bytes());
    }
}
