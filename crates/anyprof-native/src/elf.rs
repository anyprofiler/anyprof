//! ELF64 symbol tables for native stack frames.
//!
//! The Linux sampler reads `/proc/<pid>/maps` for each file's load address,
//! then reads that file's symbol table. Function names come from `SHT_SYMTAB`
//! (the symbols `nm` prints), falling back to `SHT_DYNSYM` when the binary is
//! stripped. Itanium C++ names and Rust names are demangled. DWARF is not required.
//!
//! A table above 2,000,000 symbols or a string table above 64 MiB is rejected.
//! The caller still keeps the mapping's address range, so the frame names the
//! module and the address.

#![cfg_attr(
    not(all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    )),
    allow(dead_code)
)]

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::Arc;

use anyprof_core::Runtime;

use crate::dwarf_line::{
    DwarfLineSections, LineTable, MAX_LINE_SECTION_BYTES, MAX_STR_SECTION_BYTES,
};
use crate::policy::symbol_lookup_address;
use crate::symbol::{self, frame_runtime};

/// How far past a symbol an instruction can sit and still belong to it.
const MAX_SYMBOL_SPAN: u64 = 16 * 1024 * 1024;
const MAX_SYMBOLS: u64 = 2_000_000;
const MAX_STRINGS: u64 = 64 * 1024 * 1024;

const ELF_MAGIC: [u8; 4] = [0x7f, b'E', b'L', b'F'];
const ELFCLASS64: u8 = 2;
const ELFDATA2LSB: u8 = 1;
#[allow(dead_code)]
pub(crate) const EM_X86_64: u16 = 62;
#[allow(dead_code)]
pub(crate) const EM_AARCH64: u16 = 183;
#[cfg(target_arch = "aarch64")]
pub(crate) const HOST_ELF_MACHINE: u16 = EM_AARCH64;
#[cfg(not(target_arch = "aarch64"))]
pub(crate) const HOST_ELF_MACHINE: u16 = EM_X86_64;
const ET_EXEC: u16 = 2;
const ET_DYN: u16 = 3;
const PT_LOAD: u32 = 1;
const PT_GNU_EH_FRAME: u32 = 0x6474_e550;
const SHT_SYMTAB: u32 = 2;
const SHT_STRTAB: u32 = 3;
const SHT_NOBITS: u32 = 8;
const SHT_DYNSYM: u32 = 11;
const SHF_COMPRESSED: u64 = 0x800;
const STT_FUNC: u8 = 2;
const STT_GNU_IFUNC: u8 = 10;
const SHN_UNDEF: u16 = 0;

const MAX_EH_FRAME_HDR_BYTES: u64 = 16 * 1024 * 1024;
const MAX_EH_FRAME_BYTES: u64 = 64 * 1024 * 1024;

/// Link-time DWARF CFI sections used by `framehop` to unwind without frame pointers.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ElfUnwind {
    /// Minimum `PT_LOAD.vaddr` (`0` for `ET_DYN`, e.g. `0x400000` for `ET_EXEC`).
    pub base_svma: u64,
    pub text_svma: Option<std::ops::Range<u64>>,
    pub got_svma: Option<std::ops::Range<u64>>,
    pub eh_frame_hdr_svma: Option<std::ops::Range<u64>>,
    pub eh_frame_hdr: Option<crate::walk::SectionBytes>,
    pub eh_frame_svma: Option<std::ops::Range<u64>>,
    pub eh_frame: Option<crate::walk::SectionBytes>,
    pub debug_frame: Option<crate::walk::SectionBytes>,
}

impl ElfUnwind {
    pub(crate) fn has_unwind_data(&self) -> bool {
        self.eh_frame.is_some() || self.debug_frame.is_some()
    }
}

/// One `PT_LOAD` segment, before ASLR.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LoadSegment {
    /// File offset of the segment.
    pub offset: u64,
    /// Virtual address the linker chose.
    pub vaddr: u64,
    /// Bytes stored in the file.
    pub filesz: u64,
}

/// A function symbol at its link-time virtual address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ElfFunc {
    /// `st_value`.
    pub vaddr: u64,
    /// `st_size`. Zero means "until the next symbol".
    pub size: u64,
    /// Demangled name when the symbol is Itanium C++ or Rust; otherwise the raw name.
    pub name: String,
    /// [`Runtime::Rust`] when `name` was Rust-mangled.
    pub runtime: Runtime,
}

/// Symbol table and load segments of one ELF file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ElfFile {
    /// `e_machine`.
    pub machine: u16,
    /// `PT_LOAD` segments.
    pub loads: Vec<LoadSegment>,
    /// Function symbols. Not sorted; [`UserImage`] sorts them after the slide.
    pub funcs: Vec<ElfFunc>,
    /// DWARF `.debug_line` address-to-line table at link-time addresses.
    pub lines: Arc<LineTable>,
    /// DWARF `.eh_frame` / `.eh_frame_hdr` / `.debug_frame` unwind tables.
    pub unwind: Arc<ElfUnwind>,
}

/// One loaded image and the symbols we could read for it.
#[derive(Debug, Clone)]
pub(crate) struct UserImage {
    /// Lowest mapped address of the file.
    pub start: u64,
    /// First address past the file's mappings.
    pub end: u64,
    /// File name (`busy`, `libc.so.6`, ...).
    pub module: String,
    /// Executable `(start, end)` VMA spans from `/proc/<pid>/maps`.
    pub(crate) exec_spans: Vec<(u64, u64)>,
    /// The file contains a Rust image marker. Demangled `::` paths in it are
    /// Rust frames when the mangled name is no longer available.
    pub(crate) rust_image: bool,
    /// Go pclntab for this file, relocated to the mapped address. `None` when
    /// the file is not a Go executable.
    pub(crate) go: Option<crate::go_sym::GoImage>,
    /// Runtime load bias (`runtime - link address`).
    bias: u64,
    /// `(runtime address, size, name, symbol runtime)`, sorted by address.
    symbols: Vec<(u64, u64, String, Runtime)>,
    /// Unslid DWARF line table shared with [`ElfFile`].
    lines: Arc<LineTable>,
    /// Unslid DWARF CFI unwind sections shared with [`ElfFile`].
    unwind: Arc<ElfUnwind>,
}

impl UserImage {
    /// An image whose file had no usable symbol table.
    pub(crate) fn range_only(start: u64, end: u64, module: String) -> Self {
        Self {
            start,
            end,
            module,
            exec_spans: Vec::new(),
            rust_image: false,
            go: None,
            bias: 0,
            symbols: Vec::new(),
            lines: Arc::new(LineTable::default()),
            unwind: Arc::new(ElfUnwind::default()),
        }
    }

    /// Apply `bias` (`runtime - link address`) to [`ElfFile::funcs`].
    pub(crate) fn from_elf(start: u64, end: u64, module: String, elf: &ElfFile, bias: u64) -> Self {
        let mut symbols = Vec::with_capacity(elf.funcs.len());
        for func in &elf.funcs {
            if func.name.is_empty() || func.vaddr == 0 {
                continue;
            }
            symbols.push((
                func.vaddr.wrapping_add(bias),
                func.size,
                func.name.clone(),
                func.runtime,
            ));
        }
        symbols.sort_by_key(|(addr, _, _, _)| *addr);
        symbols.dedup_by(|right, left| right.0 == left.0 && right.2 == left.2);
        Self {
            start,
            end,
            module,
            exec_spans: vec![(start, end)],
            rust_image: false,
            go: None,
            bias,
            symbols,
            lines: Arc::clone(&elf.lines),
            unwind: Arc::clone(&elf.unwind),
        }
    }

    /// Register each executable VMA span with `unwinder` when `.eh_frame` or
    /// `.debug_frame` is available.
    pub(crate) fn register_unwind_modules<U>(&self, unwinder: &mut U)
    where
        U: framehop::Unwinder<Module = framehop::Module<crate::walk::SectionBytes>>,
    {
        if !self.unwind.has_unwind_data() {
            return;
        }
        let base_avma = self.unwind.base_svma.wrapping_add(self.bias);
        for &(span_start, span_end) in &self.exec_spans {
            if span_end <= span_start || span_start < base_avma {
                continue;
            }
            let info = framehop::ExplicitModuleSectionInfo {
                base_svma: self.unwind.base_svma,
                text_svma: self.unwind.text_svma.clone(),
                got_svma: self.unwind.got_svma.clone(),
                eh_frame_hdr_svma: self.unwind.eh_frame_hdr_svma.clone(),
                eh_frame_hdr: self.unwind.eh_frame_hdr.clone(),
                eh_frame_svma: self.unwind.eh_frame_svma.clone(),
                eh_frame: self.unwind.eh_frame.clone(),
                debug_frame: self.unwind.debug_frame.clone(),
                ..Default::default()
            };
            unwinder.add_module(framehop::Module::new(
                self.module.clone(),
                span_start..span_end,
                base_avma,
                info,
            ));
        }
    }

    /// Whether `addr` falls inside this file's mappings.
    pub(crate) fn contains(&self, addr: u64) -> bool {
        addr >= self.start && addr < self.end
    }

    /// Function name and symbol runtime for `addr`, when a symbol covers it.
    pub(crate) fn name_at(&self, addr: u64) -> Option<(&str, Runtime)> {
        let idx = self
            .symbols
            .partition_point(|(start, _, _, _)| *start <= addr);
        let (start, size, name, runtime) = self.symbols.get(idx.checked_sub(1)?)?;
        let next = self
            .symbols
            .get(idx)
            .map(|(next, _, _, _)| *next)
            .unwrap_or(start.saturating_add(MAX_SYMBOL_SPAN));
        let sized_end = if *size == 0 {
            next
        } else {
            start.saturating_add(*size).min(next)
        };
        let end = sized_end.min(start.saturating_add(MAX_SYMBOL_SPAN));
        if addr >= end || addr.saturating_sub(*start) > MAX_SYMBOL_SPAN {
            return None;
        }
        Some((name.as_str(), *runtime))
    }

    /// Source file path and 1-based line number for `addr`, when `.debug_line` covers it.
    pub(crate) fn location_at(&self, addr: u64) -> Option<(&str, u32)> {
        if !self.contains(addr) {
            return None;
        }
        self.lines.lookup(addr.wrapping_sub(self.bias))
    }
}

/// Resolve `addr` for a leaf or a saved return address.
///
/// Non-leaf frames use [`symbol_lookup_address`]. The raw `addr` is tried
/// when the adjusted address falls in a gap between symbols.
///
/// The runtime is Rust when the symbol was Rust-mangled, or when the image
/// is a Rust binary and the name is an undecorated `crate::item` path.
/// Itanium C++ keeps a parameter list and stays native.
pub(crate) fn name_for(
    images: &[UserImage],
    addr: u64,
    leaf: bool,
) -> Option<(&str, &str, Runtime)> {
    let lookup = symbol_lookup_address(addr, leaf);
    find(images, lookup).or_else(|| find(images, addr))
}

fn find(images: &[UserImage], addr: u64) -> Option<(&str, &str, Runtime)> {
    let image = images.iter().find(|image| image.contains(addr))?;
    let (name, runtime) = image.name_at(addr)?;
    let runtime = frame_runtime(runtime, name, false, image.rust_image);
    Some((name, image.module.as_str(), runtime))
}

/// Resolve DWARF `(file, line)` for `addr` across loaded ELF images.
pub(crate) fn location_for(images: &[UserImage], addr: u64, leaf: bool) -> Option<(&str, u32)> {
    let lookup = symbol_lookup_address(addr, leaf);
    find_location(images, lookup).or_else(|| find_location(images, addr))
}

fn find_location(images: &[UserImage], addr: u64) -> Option<(&str, u32)> {
    let image = images.iter().find(|image| image.contains(addr))?;
    image.location_at(addr)
}

/// Runtime OS page size in bytes (`4096`, `16384`, or `65536` on Linux `aarch64`).
#[cfg(target_os = "linux")]
pub(crate) fn system_page_size() -> u64 {
    let n = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if n > 0 && (n as u64).is_power_of_two() {
        n as u64
    } else {
        4096
    }
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn system_page_size() -> u64 {
    4096
}

/// Runtime load bias: `map_start - link_address_of_that_file_offset`.
///
/// The mapping's file offset is aligned to the kernel's `page_size` (4K on
/// x86_64; 4K, 16K, or 64K on Linux aarch64). rustc (lld) emits an executable
/// segment whose `p_offset` sits mid-page, so the kernel maps file offset
/// `p_offset & !(page_size - 1)`. That offset can sit inside the previous
/// segment's `p_filesz`. Match the segment whose own page contains this file
/// offset first.
pub(crate) fn load_bias(
    map_start: u64,
    map_offset: u64,
    loads: &[LoadSegment],
    page_size: u64,
) -> Option<u64> {
    let page = if page_size >= 4096 && page_size.is_power_of_two() {
        page_size
    } else {
        4096
    };
    let load = loads
        .iter()
        .find(|load| load.filesz > 0 && (load.offset & !(page - 1)) == map_offset)
        .or_else(|| {
            loads.iter().find(|load| {
                let end = load.offset.saturating_add(load.filesz.max(1));
                map_offset >= load.offset && map_offset < end
            })
        })?;
    // The mapped page can start before `p_offset` (the rustc layout above).
    let vaddr = if map_offset >= load.offset {
        load.vaddr.wrapping_add(map_offset - load.offset)
    } else {
        load.vaddr.wrapping_sub(load.offset - map_offset)
    };
    Some(map_start.wrapping_sub(vaddr))
}

/// Read an ELF64 symbol table from `path`.
pub(crate) fn parse_path(path: &Path) -> Option<ElfFile> {
    let file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    parse_reader(file, len)
}

/// Parse `bytes` as an ELF64 object.
#[cfg(test)]
pub(crate) fn parse_bytes(bytes: &[u8]) -> Option<ElfFile> {
    parse_reader(std::io::Cursor::new(bytes), bytes.len() as u64)
}

fn parse_reader<R: Read + Seek>(mut reader: R, len: u64) -> Option<ElfFile> {
    if len < 64 {
        return None;
    }
    reader.seek(SeekFrom::Start(0)).ok()?;
    let mut ehdr = [0u8; 64];
    reader.read_exact(&mut ehdr).ok()?;
    if ehdr[0..4] != ELF_MAGIC || ehdr[4] != ELFCLASS64 || ehdr[5] != ELFDATA2LSB {
        return None;
    }
    let etype = u16::from_le_bytes(ehdr[16..18].try_into().ok()?);
    if etype != ET_EXEC && etype != ET_DYN {
        return None;
    }
    let machine = u16::from_le_bytes(ehdr[18..20].try_into().ok()?);
    let phoff = u64::from_le_bytes(ehdr[32..40].try_into().ok()?);
    let shoff = u64::from_le_bytes(ehdr[40..48].try_into().ok()?);
    let phentsize = u16::from_le_bytes(ehdr[54..56].try_into().ok()?) as u64;
    let phnum = u16::from_le_bytes(ehdr[56..58].try_into().ok()?) as u64;
    let shentsize = u16::from_le_bytes(ehdr[58..60].try_into().ok()?) as u64;
    let mut shnum = u16::from_le_bytes(ehdr[60..62].try_into().ok()?) as u64;
    let mut shstrndx = u16::from_le_bytes(ehdr[62..64].try_into().ok()?) as u64;
    if phentsize < 56 || shentsize < 64 {
        return None;
    }
    let (loads, gnu_eh_frame) = read_loads(&mut reader, len, phoff, phentsize, phnum)?;
    if shoff == 0 {
        let unwind = Arc::new(read_elf_unwind_from_phdr(
            &mut reader,
            len,
            &loads,
            gnu_eh_frame,
        ));
        return Some(ElfFile {
            machine,
            loads,
            funcs: Vec::new(),
            lines: Arc::new(LineTable::default()),
            unwind,
        });
    }
    if shnum == 0 {
        let section0 = read_section(&mut reader, len, shoff, shentsize, 0)?;
        shnum = section0.size;
        if shstrndx == 0xffff {
            shstrndx = u64::from(section0.link);
        }
    }
    if shnum == 0 || shnum > 100_000 {
        return None;
    }
    let sections = read_sections(&mut reader, len, shoff, shentsize, shnum)?;
    let funcs = read_funcs(&mut reader, len, &sections);
    let lines = read_dwarf_lines(&mut reader, len, &sections, shstrndx);
    let unwind = Arc::new(read_elf_unwind(
        &mut reader,
        len,
        &loads,
        gnu_eh_frame,
        &sections,
        shstrndx,
    ));
    Some(ElfFile {
        machine,
        loads,
        funcs,
        lines,
        unwind,
    })
}

#[derive(Clone, Copy)]
struct Section {
    name: u32,
    kind: u32,
    flags: u64,
    addr: u64,
    offset: u64,
    size: u64,
    link: u32,
    entsize: u64,
}

fn read_loads<R: Read + Seek>(
    reader: &mut R,
    len: u64,
    phoff: u64,
    entsize: u64,
    phnum: u64,
) -> Option<(Vec<LoadSegment>, Option<LoadSegment>)> {
    if phnum > 1000 || phoff.saturating_add(phnum.saturating_mul(entsize)) > len {
        return None;
    }
    let mut loads = Vec::new();
    let mut gnu_eh_frame = None;
    for index in 0..phnum {
        let mut buf = vec![0u8; entsize as usize];
        reader.seek(SeekFrom::Start(phoff + index * entsize)).ok()?;
        reader.read_exact(&mut buf).ok()?;
        let kind = u32::from_le_bytes(buf[0..4].try_into().ok()?);
        if kind != PT_LOAD && kind != PT_GNU_EH_FRAME {
            continue;
        }
        let offset = u64::from_le_bytes(buf[8..16].try_into().ok()?);
        let vaddr = u64::from_le_bytes(buf[16..24].try_into().ok()?);
        let filesz = u64::from_le_bytes(buf[32..40].try_into().ok()?);
        let seg = LoadSegment {
            offset,
            vaddr,
            filesz,
        };
        if kind == PT_LOAD {
            loads.push(seg);
        } else if gnu_eh_frame.is_none() && filesz > 0 {
            gnu_eh_frame = Some(seg);
        }
    }
    Some((loads, gnu_eh_frame))
}

fn read_sections<R: Read + Seek>(
    reader: &mut R,
    len: u64,
    shoff: u64,
    entsize: u64,
    shnum: u64,
) -> Option<Vec<Section>> {
    let total = shnum.checked_mul(entsize)?;
    if total > 64 * 1024 * 1024 || shoff.saturating_add(total) > len {
        return None;
    }
    let mut raw = vec![0u8; total as usize];
    reader.seek(SeekFrom::Start(shoff)).ok()?;
    reader.read_exact(&mut raw).ok()?;
    let step = entsize as usize;
    let mut sections = Vec::with_capacity(shnum as usize);
    for index in 0..shnum as usize {
        let buf = &raw[index * step..index * step + 64];
        sections.push(parse_section_bytes(buf)?);
    }
    Some(sections)
}

fn read_section<R: Read + Seek>(
    reader: &mut R,
    len: u64,
    shoff: u64,
    entsize: u64,
    index: u64,
) -> Option<Section> {
    let at = shoff.saturating_add(index.saturating_mul(entsize));
    if at.saturating_add(64) > len {
        return None;
    }
    let mut buf = [0u8; 64];
    reader.seek(SeekFrom::Start(at)).ok()?;
    reader.read_exact(&mut buf).ok()?;
    parse_section_bytes(&buf)
}

fn parse_section_bytes(buf: &[u8]) -> Option<Section> {
    Some(Section {
        name: u32::from_le_bytes(buf[0..4].try_into().ok()?),
        kind: u32::from_le_bytes(buf[4..8].try_into().ok()?),
        flags: u64::from_le_bytes(buf[8..16].try_into().ok()?),
        addr: u64::from_le_bytes(buf[16..24].try_into().ok()?),
        offset: u64::from_le_bytes(buf[24..32].try_into().ok()?),
        size: u64::from_le_bytes(buf[32..40].try_into().ok()?),
        link: u32::from_le_bytes(buf[40..44].try_into().ok()?),
        entsize: u64::from_le_bytes(buf[56..64].try_into().ok()?),
    })
}

fn read_funcs<R: Read + Seek>(reader: &mut R, len: u64, sections: &[Section]) -> Vec<ElfFunc> {
    let symtab = sections
        .iter()
        .find(|section| section.kind == SHT_SYMTAB)
        .or_else(|| sections.iter().find(|section| section.kind == SHT_DYNSYM));
    let Some(symtab) = symtab else {
        return Vec::new();
    };
    let Some(strings) = sections.get(symtab.link as usize) else {
        return Vec::new();
    };
    if strings.kind != SHT_STRTAB && strings.kind != 0 {
        // A symtab's link is the string table. Kind 0 only happens in tests
        // that omit it; a real file uses SHT_STRTAB.
    }
    if strings.size > MAX_STRINGS || strings.offset.saturating_add(strings.size) > len {
        return Vec::new();
    }
    let entsize = if symtab.entsize == 0 {
        24
    } else {
        symtab.entsize
    };
    if entsize < 24 || symtab.size / entsize > MAX_SYMBOLS {
        return Vec::new();
    }
    if symtab.offset.saturating_add(symtab.size) > len {
        return Vec::new();
    }
    let mut strtab = vec![0u8; strings.size as usize];
    if reader.seek(SeekFrom::Start(strings.offset)).is_err()
        || reader.read_exact(&mut strtab).is_err()
    {
        return Vec::new();
    }
    let mut symbytes = vec![0u8; symtab.size as usize];
    if reader.seek(SeekFrom::Start(symtab.offset)).is_err()
        || reader.read_exact(&mut symbytes).is_err()
    {
        return Vec::new();
    }
    let step = entsize as usize;
    let count = symbytes.len() / step;
    let mut funcs = Vec::new();
    for index in 0..count {
        let ent = &symbytes[index * step..index * step + 24];
        let name_off = u32::from_le_bytes(ent[0..4].try_into().unwrap()) as usize;
        let info = ent[4];
        let shndx = u16::from_le_bytes(ent[6..8].try_into().unwrap());
        let value = u64::from_le_bytes(ent[8..16].try_into().unwrap());
        let size = u64::from_le_bytes(ent[16..24].try_into().unwrap());
        let kind = info & 0xf;
        if shndx == SHN_UNDEF || value == 0 || (kind != STT_FUNC && kind != STT_GNU_IFUNC) {
            continue;
        }
        let Some(raw) = cstr_at(&strtab, name_off) else {
            continue;
        };
        if raw.is_empty() {
            continue;
        }
        let presented = symbol::present_elf_symbol(raw);
        funcs.push(ElfFunc {
            vaddr: value,
            size,
            name: presented.name,
            runtime: presented.runtime,
        });
    }
    funcs
}

fn read_dwarf_lines<R: Read + Seek>(
    reader: &mut R,
    len: u64,
    sections: &[Section],
    shstrndx: u64,
) -> Arc<LineTable> {
    let Ok(shstr_idx) = usize::try_from(shstrndx) else {
        return Arc::new(LineTable::default());
    };
    let Some(shstr_sec) = sections.get(shstr_idx) else {
        return Arc::new(LineTable::default());
    };
    let Some(shstr) = read_section_bytes(reader, len, *shstr_sec, MAX_STRINGS) else {
        return Arc::new(LineTable::default());
    };
    let mut line_sec = None;
    let mut line_str_sec = None;
    let mut str_sec = None;
    for section in sections {
        if section.kind == SHT_NOBITS || (section.flags & SHF_COMPRESSED) != 0 || section.size == 0
        {
            continue;
        }
        let Some(name) = cstr_at(&shstr, section.name as usize) else {
            continue;
        };
        match name {
            ".debug_line" if line_sec.is_none() => line_sec = Some(*section),
            ".debug_line_str" if line_str_sec.is_none() => line_str_sec = Some(*section),
            ".debug_str" if str_sec.is_none() => str_sec = Some(*section),
            _ => {}
        }
    }
    let Some(line_sec) = line_sec else {
        return Arc::new(LineTable::default());
    };
    let Some(debug_line) = read_section_bytes(reader, len, line_sec, MAX_LINE_SECTION_BYTES) else {
        return Arc::new(LineTable::default());
    };
    let debug_line_str = line_str_sec
        .and_then(|sec| read_section_bytes(reader, len, sec, MAX_STR_SECTION_BYTES))
        .unwrap_or_default();
    let debug_str = str_sec
        .and_then(|sec| read_section_bytes(reader, len, sec, MAX_STR_SECTION_BYTES))
        .unwrap_or_default();
    Arc::new(LineTable::parse(
        DwarfLineSections {
            debug_line: &debug_line,
            debug_line_str: &debug_line_str,
            debug_str: &debug_str,
        },
        8,
    ))
}

fn read_elf_unwind<R: Read + Seek>(
    reader: &mut R,
    len: u64,
    loads: &[LoadSegment],
    gnu_eh_frame: Option<LoadSegment>,
    sections: &[Section],
    shstrndx: u64,
) -> ElfUnwind {
    let base_svma = loads.iter().map(|load| load.vaddr).min().unwrap_or(0);
    let shstr = usize::try_from(shstrndx)
        .ok()
        .and_then(|idx| sections.get(idx).copied())
        .and_then(|sec| read_section_bytes(reader, len, sec, MAX_STRINGS));
    let mut text_svma = None;
    let mut got_svma = None;
    let mut got_plt_svma = None;
    let mut eh_hdr_sec = None;
    let mut eh_sec = None;
    let mut debug_frame_sec = None;
    if let Some(shstr) = shstr.as_deref() {
        for section in sections {
            if section.kind == SHT_NOBITS
                || (section.flags & SHF_COMPRESSED) != 0
                || section.size == 0
            {
                continue;
            }
            let Some(name) = cstr_at(shstr, section.name as usize) else {
                continue;
            };
            let range = section.addr..section.addr.saturating_add(section.size);
            match name {
                ".text" if text_svma.is_none() => text_svma = Some(range),
                ".got" if got_svma.is_none() => got_svma = Some(range),
                ".got.plt" if got_plt_svma.is_none() => got_plt_svma = Some(range),
                ".eh_frame_hdr" if eh_hdr_sec.is_none() => eh_hdr_sec = Some(*section),
                ".eh_frame" if eh_sec.is_none() => eh_sec = Some(*section),
                ".debug_frame" if debug_frame_sec.is_none() => debug_frame_sec = Some(*section),
                _ => {}
            }
        }
    }
    let mut eh_frame_hdr_svma = None;
    let mut eh_frame_hdr = None;
    if let Some(sec) = eh_hdr_sec
        && let Some(bytes) = read_section_bytes(reader, len, sec, MAX_EH_FRAME_HDR_BYTES)
    {
        eh_frame_hdr_svma = Some(sec.addr..sec.addr.saturating_add(sec.size));
        eh_frame_hdr = Some(crate::walk::SectionBytes::new(bytes));
    }
    let mut eh_frame_svma = None;
    let mut eh_frame = None;
    if let Some(sec) = eh_sec
        && let Some(bytes) = read_section_bytes(reader, len, sec, MAX_EH_FRAME_BYTES)
    {
        eh_frame_svma = Some(sec.addr..sec.addr.saturating_add(sec.size));
        eh_frame = Some(crate::walk::SectionBytes::new(bytes));
    }
    if eh_frame.is_none() {
        let fallback = read_elf_unwind_from_phdr(reader, len, loads, gnu_eh_frame);
        if fallback.eh_frame.is_some() {
            eh_frame_hdr_svma = fallback.eh_frame_hdr_svma;
            eh_frame_hdr = fallback.eh_frame_hdr;
            eh_frame_svma = fallback.eh_frame_svma;
            eh_frame = fallback.eh_frame;
        }
    }
    let debug_frame = if eh_frame.is_none() {
        debug_frame_sec
            .and_then(|sec| read_section_bytes(reader, len, sec, MAX_EH_FRAME_BYTES))
            .map(crate::walk::SectionBytes::new)
    } else {
        None
    };
    ElfUnwind {
        base_svma,
        text_svma,
        got_svma: got_svma.or(got_plt_svma),
        eh_frame_hdr_svma,
        eh_frame_hdr,
        eh_frame_svma,
        eh_frame,
        debug_frame,
    }
}

fn read_elf_unwind_from_phdr<R: Read + Seek>(
    reader: &mut R,
    len: u64,
    loads: &[LoadSegment],
    gnu_eh_frame: Option<LoadSegment>,
) -> ElfUnwind {
    let base_svma = loads.iter().map(|load| load.vaddr).min().unwrap_or(0);
    let Some(hdr_seg) = gnu_eh_frame else {
        return ElfUnwind {
            base_svma,
            ..Default::default()
        };
    };
    let Some(hdr_bytes) = read_range_bytes(
        reader,
        len,
        hdr_seg.offset,
        hdr_seg.filesz,
        MAX_EH_FRAME_HDR_BYTES,
    ) else {
        return ElfUnwind {
            base_svma,
            ..Default::default()
        };
    };
    let Some(eh_vaddr) = decode_eh_frame_hdr_ptr(&hdr_bytes, hdr_seg.vaddr) else {
        return ElfUnwind {
            base_svma,
            ..Default::default()
        };
    };
    let Some(load) = loads
        .iter()
        .find(|load| eh_vaddr >= load.vaddr && eh_vaddr < load.vaddr.saturating_add(load.filesz))
    else {
        return ElfUnwind {
            base_svma,
            ..Default::default()
        };
    };
    let off_in_load = eh_vaddr - load.vaddr;
    let eh_offset = load.offset.saturating_add(off_in_load);
    let eh_size = (load.filesz - off_in_load).min(MAX_EH_FRAME_BYTES);
    let Some(eh_bytes) = read_range_bytes(reader, len, eh_offset, eh_size, MAX_EH_FRAME_BYTES)
    else {
        return ElfUnwind {
            base_svma,
            ..Default::default()
        };
    };
    ElfUnwind {
        base_svma,
        eh_frame_hdr_svma: Some(hdr_seg.vaddr..hdr_seg.vaddr.saturating_add(hdr_seg.filesz)),
        eh_frame_hdr: Some(crate::walk::SectionBytes::new(hdr_bytes)),
        eh_frame_svma: Some(eh_vaddr..eh_vaddr.saturating_add(eh_size)),
        eh_frame: Some(crate::walk::SectionBytes::new(eh_bytes)),
        ..Default::default()
    }
}

fn decode_eh_frame_hdr_ptr(hdr: &[u8], hdr_vaddr: u64) -> Option<u64> {
    if hdr.len() < 8 || hdr[0] != 1 {
        return None;
    }
    let enc = hdr[1];
    if enc == 0xff || (enc & 0x80) != 0 {
        return None;
    }
    let raw = match enc & 0x0f {
        0x00 | 0x04 => u64::from_le_bytes(hdr.get(4..12)?.try_into().ok()?),
        0x03 => u32::from_le_bytes(hdr.get(4..8)?.try_into().ok()?) as u64,
        0x0b => (i32::from_le_bytes(hdr.get(4..8)?.try_into().ok()?) as i64) as u64,
        0x0c => i64::from_le_bytes(hdr.get(4..12)?.try_into().ok()?) as u64,
        _ => return None,
    };
    match enc & 0x70 {
        0x00 => Some(raw),
        0x10 => Some(hdr_vaddr.wrapping_add(4).wrapping_add(raw)),
        0x30 => Some(hdr_vaddr.wrapping_add(raw)),
        _ => None,
    }
}

fn read_section_bytes<R: Read + Seek>(
    reader: &mut R,
    len: u64,
    section: Section,
    cap: u64,
) -> Option<Vec<u8>> {
    read_range_bytes(reader, len, section.offset, section.size, cap)
}

fn read_range_bytes<R: Read + Seek>(
    reader: &mut R,
    len: u64,
    offset: u64,
    size: u64,
    cap: u64,
) -> Option<Vec<u8>> {
    if size == 0 || size > cap || offset.saturating_add(size) > len {
        return None;
    }
    let mut buf = vec![0u8; size as usize];
    reader.seek(SeekFrom::Start(offset)).ok()?;
    reader.read_exact(&mut buf).ok()?;
    Some(buf)
}

fn cstr_at(buf: &[u8], offset: usize) -> Option<&str> {
    let rest = buf.get(offset..)?;
    let end = rest
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(rest.len());
    std::str::from_utf8(&rest[..end]).ok()
}

/// Whether this ELF matches the host Linux sampler architecture (`x86_64` or `aarch64`).
pub(crate) fn is_host_linux_elf(elf: &ElfFile) -> bool {
    elf.machine == HOST_ELF_MACHINE
}

#[cfg(test)]
mod tests {
    use super::{EM_AARCH64, EM_X86_64, UserImage, load_bias, name_for, parse_bytes};

    #[test]
    fn itanium_names_are_demangled_and_c_names_stay() {
        let pretty = crate::symbol::present_elf_symbol("_ZN14anyprof_native4spinEv");
        assert_eq!(pretty.runtime, anyprof_core::Runtime::Native);
        assert!(
            pretty.name.contains("anyprof_native") && pretty.name.contains("spin"),
            "{}",
            pretty.name
        );
        assert_eq!(crate::symbol::present_elf_symbol("main").name, "main");
        assert_eq!(
            crate::symbol::present_elf_symbol("read@@GLIBC_2.2.5").name,
            "read"
        );
    }

    #[test]
    fn a_return_address_on_the_next_symbol_stays_in_the_caller() {
        let bytes = synthetic_elf(&[
            ("_ZN14anyprof_native4spinEv", 0x1000, 0x200),
            ("main", 0x1200, 0x40),
        ]);
        let elf = parse_bytes(&bytes).expect("synthetic elf");
        assert_eq!(elf.machine, EM_X86_64);
        let bias = load_bias(0x5555_0000_0000, 0, &elf.loads, 4096).unwrap();
        assert_eq!(bias, 0x5555_0000_0000);
        let image = UserImage::from_elf(bias, bias + 0x2000, "busy".to_string(), &elf, bias);
        let (name, module, runtime) =
            name_for(std::slice::from_ref(&image), bias + 0x1050, true).unwrap();
        assert!(name.contains("spin"), "{name}");
        assert_eq!(module, "busy");
        assert_eq!(runtime, anyprof_core::Runtime::Native);
        // 0x1200 is the first byte of main. A non-leaf sample of that address
        // is the call in spin, not main itself.
        let (name, _, _) = name_for(std::slice::from_ref(&image), bias + 0x1200, false).unwrap();
        assert!(name.contains("spin"), "{name}");
        let (name, _, _) = name_for(std::slice::from_ref(&image), bias + 0x1200, true).unwrap();
        assert_eq!(name, "main");
        assert!(name_for(std::slice::from_ref(&image), bias + 0x10, true).is_none());
    }

    #[test]
    fn rustc_executable_segment_is_not_shifted_by_a_page() {
        // rustc/lld: the executable PT_LOAD starts at file offset 0x15820 /
        // vaddr 0x16820. Linux maps that page at file offset 0x15000, which
        // is still inside the previous segment's p_filesz (0x15814).
        let loads = [
            super::LoadSegment {
                offset: 0,
                vaddr: 0,
                filesz: 0x15814,
            },
            super::LoadSegment {
                offset: 0x15820,
                vaddr: 0x16820,
                filesz: 0x3d570,
            },
        ];
        let map_start = 0x5555_0008_8000u64;
        let bias = load_bias(map_start, 0x15000, &loads, 4096).unwrap();
        assert_eq!(bias, map_start - 0x16000);
        // Link vaddr 0x17440 is file offset 0x16440, 0x1440 bytes into the map.
        assert_eq!(bias.wrapping_add(0x17440), map_start + 0x1440);
    }

    #[test]
    fn aarch64_elf_and_16k_64k_page_alignments_resolve_load_bias() {
        let mut bytes = synthetic_elf(&[("arm64_leaf", 0x2000, 0x80)]);
        bytes[18..20].copy_from_slice(&EM_AARCH64.to_le_bytes());
        let elf = parse_bytes(&bytes).expect("synthetic arm64 elf");
        assert_eq!(elf.machine, EM_AARCH64);

        // 16 KiB pages (0x4000): segment at file offset 0x16820 / vaddr 0x26820
        // maps from page offset 0x14000, inside the previous segment's filesz (0x16810).
        let loads_16k = [
            super::LoadSegment {
                offset: 0,
                vaddr: 0,
                filesz: 0x16810,
            },
            super::LoadSegment {
                offset: 0x16820,
                vaddr: 0x26820,
                filesz: 0x20000,
            },
        ];
        let map_16k = 0x0000_aaaa_c001_4000u64;
        let bias_16k = load_bias(map_16k, 0x14000, &loads_16k, 16 * 1024).unwrap();
        assert_eq!(bias_16k, map_16k - 0x24000);

        // 64 KiB pages (0x1_0000): segment at file offset 0x2a400 / vaddr 0x3a400
        // maps from page offset 0x20000, inside the previous segment's filesz (0x2a3f0).
        let loads_64k = [
            super::LoadSegment {
                offset: 0,
                vaddr: 0,
                filesz: 0x2a3f0,
            },
            super::LoadSegment {
                offset: 0x2a400,
                vaddr: 0x3a400,
                filesz: 0x40000,
            },
        ];
        let map_64k = 0x0000_aaaa_c002_0000u64;
        let bias_64k = load_bias(map_64k, 0x20000, &loads_64k, 64 * 1024).unwrap();
        assert_eq!(bias_64k, map_64k - 0x30000);
    }

    #[test]
    fn garbage_is_not_an_elf() {
        assert!(parse_bytes(&[]).is_none());
        assert!(parse_bytes(b"not elf").is_none());
        assert!(parse_bytes(&[0x7f, b'E', b'L', b'F', 1, 1]).is_none());
    }

    #[cfg(all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    #[test]
    fn compiled_elf_with_debug_info_resolves_source_file_and_line() {
        let dir = std::env::temp_dir().join(format!("anyprof-elf-dwarf-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("sample.cpp");
        let bin = dir.join("sample");
        std::fs::write(
            &src,
            "int target_fn(int x) {\n    return x * 3 + 1;\n}\nint main() {\n    return target_fn(5);\n}\n",
        )
        .unwrap();
        let status = std::process::Command::new("g++")
            .args(["-O0", "-g", "-o"])
            .arg(&bin)
            .arg(&src)
            .status();
        let Ok(status) = status else {
            let _ = std::fs::remove_dir_all(&dir);
            return;
        };
        if !status.success() {
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
        let elf = super::parse_path(&bin).expect("parse compiled elf");
        let target = elf
            .funcs
            .iter()
            .find(|func| func.name.contains("target_fn"))
            .expect("target_fn symbol");
        let bias = 0x5555_0000_0000u64;
        let image = UserImage::from_elf(bias, bias + 0x10_0000, "sample".to_string(), &elf, bias);
        let (file, line) =
            super::location_for(std::slice::from_ref(&image), bias + target.vaddr + 4, true)
                .expect("dwarf location for target_fn");
        assert!(file.ends_with("sample.cpp"), "unexpected file: {file}");
        assert!((1..=3).contains(&line), "unexpected line: {line}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    #[test]
    fn compiled_fp_less_elf_exposes_eh_frame_and_phdr_fallback() {
        use framehop::Unwinder as _;
        let dir = std::env::temp_dir().join(format!("anyprof-elf-ehframe-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("nofp.cpp");
        let bin = dir.join("nofp");
        std::fs::write(
            &src,
            "__attribute__((noinline)) int leaf_fn(int x) {\n    volatile int y = x + 3;\n    return y;\n}\n__attribute__((noinline)) int mid_fn(int x) {\n    return leaf_fn(x) + 1;\n}\nint main() {\n    return mid_fn(4);\n}\n",
        )
        .unwrap();
        let status = std::process::Command::new("g++")
            .args([
                "-O2",
                "-fomit-frame-pointer",
                "-fno-optimize-sibling-calls",
                "-o",
            ])
            .arg(&bin)
            .arg(&src)
            .status();
        let Ok(status) = status else {
            let _ = std::fs::remove_dir_all(&dir);
            return;
        };
        if !status.success() {
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
        let elf = super::parse_path(&bin).expect("parse compiled nofp elf");
        assert!(elf.unwind.has_unwind_data());
        assert!(elf.unwind.eh_frame.is_some());
        assert!(elf.unwind.eh_frame_hdr.is_some());
        let bias = 0x5555_0000_0000u64;
        let image = UserImage::from_elf(bias, bias + 0x10_0000, "nofp".to_string(), &elf, bias);
        let mut unwinder = framehop::UnwinderNative::<
            crate::walk::SectionBytes,
            framehop::MayAllocateDuringUnwind,
        >::new();
        image.register_unwind_modules(&mut unwinder);
        assert!(unwinder.max_known_code_address() > bias);

        // Strip section headers (`e_shoff = 0`) to verify PT_GNU_EH_FRAME fallback.
        let mut raw = std::fs::read(&bin).unwrap();
        raw[40..48].copy_from_slice(&0u64.to_le_bytes());
        let stripped = super::parse_bytes(&raw).expect("parse sectionless elf");
        assert!(stripped.unwind.has_unwind_data());
        assert!(stripped.unwind.eh_frame.is_some());
        assert!(stripped.unwind.eh_frame_hdr.is_some());
        assert_eq!(
            stripped.unwind.eh_frame_svma.as_ref().map(|r| r.start),
            elf.unwind.eh_frame_svma.as_ref().map(|r| r.start)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn synthetic_elf(symbols: &[(&str, u64, u64)]) -> Vec<u8> {
        let mut strtab = vec![0u8];
        let mut syms = vec![0u8; 24];
        for (name, vaddr, size) in symbols {
            let off = strtab.len() as u32;
            strtab.extend(name.as_bytes());
            strtab.push(0);
            let mut ent = [0u8; 24];
            ent[0..4].copy_from_slice(&off.to_le_bytes());
            ent[4] = (1 << 4) | 2;
            ent[6..8].copy_from_slice(&1u16.to_le_bytes());
            ent[8..16].copy_from_slice(&vaddr.to_le_bytes());
            ent[16..24].copy_from_slice(&size.to_le_bytes());
            syms.extend_from_slice(&ent);
        }
        let shstr = b"\0.text\0.strtab\0.symtab\0.shstrtab\0";
        let str_off = 128u64;
        let sym_off = align8(str_off + strtab.len() as u64);
        let shstr_off = align8(sym_off + syms.len() as u64);
        let shoff = align8(shstr_off + shstr.len() as u64);
        let file_len = shoff + 5 * 64;
        let mut bytes = vec![0u8; file_len as usize];
        bytes[0..4].copy_from_slice(&super::ELF_MAGIC);
        bytes[4] = 2;
        bytes[5] = 1;
        bytes[16..18].copy_from_slice(&3u16.to_le_bytes()); // ET_DYN
        bytes[18..20].copy_from_slice(&62u16.to_le_bytes());
        bytes[32..40].copy_from_slice(&64u64.to_le_bytes()); // phoff
        bytes[40..48].copy_from_slice(&shoff.to_le_bytes());
        bytes[52..54].copy_from_slice(&64u16.to_le_bytes()); // ehsize
        bytes[54..56].copy_from_slice(&56u16.to_le_bytes());
        bytes[56..58].copy_from_slice(&1u16.to_le_bytes());
        bytes[58..60].copy_from_slice(&64u16.to_le_bytes());
        bytes[60..62].copy_from_slice(&5u16.to_le_bytes());
        bytes[62..64].copy_from_slice(&4u16.to_le_bytes());
        // PT_LOAD covering the whole file at vaddr 0.
        let ph = 64usize;
        bytes[ph..ph + 4].copy_from_slice(&1u32.to_le_bytes());
        bytes[ph + 4..ph + 8].copy_from_slice(&5u32.to_le_bytes());
        bytes[ph + 32..ph + 40].copy_from_slice(&file_len.to_le_bytes());
        bytes[ph + 40..ph + 48].copy_from_slice(&file_len.to_le_bytes());
        bytes[str_off as usize..str_off as usize + strtab.len()].copy_from_slice(&strtab);
        bytes[sym_off as usize..sym_off as usize + syms.len()].copy_from_slice(&syms);
        bytes[shstr_off as usize..shstr_off as usize + shstr.len()].copy_from_slice(shstr);
        write_section(
            &mut bytes,
            shoff,
            SectionSpec {
                index: 1,
                kind: 1,
                offset: 0,
                size: 0,
                addr: 0x1000,
                link: 0,
                entsize: 0,
                name: name_off(shstr, ".text"),
            },
        );
        write_section(
            &mut bytes,
            shoff,
            SectionSpec {
                index: 2,
                kind: 3,
                offset: str_off,
                size: strtab.len() as u64,
                addr: 0,
                link: 0,
                entsize: 0,
                name: name_off(shstr, ".strtab"),
            },
        );
        write_section(
            &mut bytes,
            shoff,
            SectionSpec {
                index: 3,
                kind: 2,
                offset: sym_off,
                size: syms.len() as u64,
                addr: 0,
                link: 2,
                entsize: 24,
                name: name_off(shstr, ".symtab"),
            },
        );
        write_section(
            &mut bytes,
            shoff,
            SectionSpec {
                index: 4,
                kind: 3,
                offset: shstr_off,
                size: shstr.len() as u64,
                addr: 0,
                link: 0,
                entsize: 0,
                name: name_off(shstr, ".shstrtab"),
            },
        );
        bytes
    }

    fn align8(value: u64) -> u64 {
        value.saturating_add(7) & !7
    }

    fn name_off(shstr: &[u8], name: &str) -> u32 {
        let text = std::str::from_utf8(shstr).unwrap();
        text.find(name).unwrap() as u32
    }

    struct SectionSpec {
        index: u64,
        kind: u32,
        offset: u64,
        size: u64,
        addr: u64,
        link: u32,
        entsize: u64,
        name: u32,
    }

    fn write_section(bytes: &mut [u8], shoff: u64, section: SectionSpec) {
        let at = (shoff + section.index * 64) as usize;
        bytes[at..at + 4].copy_from_slice(&section.name.to_le_bytes());
        bytes[at + 4..at + 8].copy_from_slice(&section.kind.to_le_bytes());
        bytes[at + 16..at + 24].copy_from_slice(&section.addr.to_le_bytes());
        bytes[at + 24..at + 32].copy_from_slice(&section.offset.to_le_bytes());
        bytes[at + 32..at + 40].copy_from_slice(&section.size.to_le_bytes());
        bytes[at + 40..at + 44].copy_from_slice(&section.link.to_le_bytes());
        bytes[at + 56..at + 64].copy_from_slice(&section.entsize.to_le_bytes());
    }
}
