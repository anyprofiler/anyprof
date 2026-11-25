//! Mach-O symbol tables for native stack frames.
//!
//! The macOS sampler reads each loaded image's header from the target, then
//! opens the on-disk binary and reads only its symbol table. Function names
//! come from `nlist_64` (the symbols `nm` prints), not from DWARF, so a normal
//! unstripped C, C++, or Rust binary resolves without `-g`. Rust names are
//! demangled.
//!
//! Addresses in the file are the linker's preferred virtual addresses. The
//! sampler adds the ASLR slide (`runtime load address - __TEXT vmaddr`) before
//! lookup. A sampled instruction pointer maps to the last symbol at or below
//! that address, as long as it is still inside the following symbol.
//!
//! Windows and Linux resolve symbols from PE and ELF instead, so this
//! module's only caller there is its unit tests. `dead_code` is allowed on
//! those builds.
//!
//! Symbol bytes are read with a seek, capped at 2,000,000 symbols and 64 MiB
//! of string table. A larger on-disk binary is still parsed. System libraries
//! inside macOS's `dyld_v1` shared cache are resolved via [`crate::dyld_cache`].

#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::Arc;

use anyprof_core::Runtime;

use crate::dwarf_line::{
    DwarfLineSections, LineTable, MAX_LINE_SECTION_BYTES, MAX_STR_SECTION_BYTES,
};
use crate::symbol::{self, Presented};

/// How far past a symbol an instruction can sit and still belong to it.
///
/// A gap larger than this is padding or an unmapped hole, not the function.
const MAX_SYMBOL_SPAN: u64 = 16 * 1024 * 1024;

const MH_MAGIC_64: u32 = 0xFEED_FACF;
pub(crate) const MH_DYLIB_IN_CACHE: u32 = 0x8000_0000;
const FAT_MAGIC: u32 = 0xCAFE_BABE;
const CPU_TYPE_X86_64: u32 = 0x0100_0007;
const CPU_TYPE_ARM64: u32 = 0x0100_000C;
const LC_SYMTAB: u32 = 0x2;
pub(crate) const LC_SEGMENT_64: u32 = 0x19;
pub(crate) const S_ZEROFILL: u32 = 0x1;
pub(crate) const N_STAB: u8 = 0xe0;
pub(crate) const N_TYPE: u8 = 0x0e;
pub(crate) const N_SECT: u8 = 0x0e;

pub(crate) const MAX_UNWIND_INFO_BYTES: u64 = 16 * 1024 * 1024;
pub(crate) const MAX_EH_FRAME_BYTES: u64 = 64 * 1024 * 1024;
pub(crate) const MAX_TEXT_UNWIND_BYTES: u64 = 8 * 1024 * 1024;

/// Link-time Mach-O Compact Unwind (`__unwind_info`) and DWARF (`__eh_frame`)
/// sections passed to `framehop::Module`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct MachoUnwind {
    pub base_svma: u64,
    pub text_svma: Option<std::ops::Range<u64>>,
    pub text: Option<crate::walk::SectionBytes>,
    pub stubs_svma: Option<std::ops::Range<u64>>,
    pub stub_helper_svma: Option<std::ops::Range<u64>>,
    pub got_svma: Option<std::ops::Range<u64>>,
    pub unwind_info: Option<crate::walk::SectionBytes>,
    pub eh_frame_svma: Option<std::ops::Range<u64>>,
    pub eh_frame: Option<crate::walk::SectionBytes>,
}

impl MachoUnwind {
    pub(crate) fn has_unwind_data(&self) -> bool {
        self.unwind_info.is_some() || self.eh_frame.is_some()
    }
}

/// CPU slice to select inside a universal (fat) binary.
///
/// `dead_code` is allowed on the variant this process is not running, because
/// `host` constructs only that architecture. Fat parsing still matches both.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MachCpu {
    /// `CPU_TYPE_ARM64`, including Apple Silicon.
    #[cfg_attr(not(target_arch = "aarch64"), allow(dead_code))]
    Arm64,
    /// `CPU_TYPE_X86_64`.
    #[cfg_attr(not(target_arch = "x86_64"), allow(dead_code))]
    X86_64,
}

impl MachCpu {
    fn cputype(self) -> u32 {
        match self {
            Self::Arm64 => CPU_TYPE_ARM64,
            Self::X86_64 => CPU_TYPE_X86_64,
        }
    }

    /// CPU of the machine this build runs on.
    ///
    /// Sampling is local, so the target was loaded as this architecture.
    #[cfg(target_os = "macos")]
    pub(crate) fn host() -> Self {
        #[cfg(target_arch = "aarch64")]
        {
            Self::Arm64
        }
        #[cfg(target_arch = "x86_64")]
        {
            Self::X86_64
        }
    }
}

/// One loaded Mach-O image and the symbols we could read for it.
#[derive(Debug, Clone)]
pub(crate) struct ImageSymbols {
    /// Runtime address of the Mach-O header.
    pub start: u64,
    /// First runtime address past the image's segments.
    pub end: u64,
    /// First runtime address past the image's executable `__TEXT` segment.
    pub(crate) text_end: u64,
    /// File name (`busy`, `libsystem_kernel.dylib`, ...).
    pub module: String,
    /// The file contains a Rust image marker.
    pub(crate) rust_image: bool,
    /// Go pclntab relocated by this image’s ASLR slide.
    pub(crate) go: Option<crate::go_sym::GoImage>,
    /// ASLR slide (`load_addr - __TEXT vmaddr`).
    slide: u64,
    /// `(runtime address, display name, symbol runtime)`, sorted by address.
    symbols: Vec<(u64, String, Runtime)>,
    /// Unslid DWARF `.debug_line` table from `__DWARF` or a sibling `.dSYM`.
    lines: Arc<LineTable>,
    /// Unslid Mach-O Compact Unwind / `__eh_frame` sections.
    unwind: Arc<MachoUnwind>,
}

impl ImageSymbols {
    /// An image whose file had no usable symbol table.
    ///
    /// Addresses inside `[start, end)` still name the module. The frame's
    /// function name stays empty and exporters print the address.
    pub(crate) fn range_only(start: u64, end: u64, module: String) -> Self {
        Self {
            start,
            end,
            text_end: end,
            module,
            rust_image: false,
            go: None,
            slide: 0,
            symbols: Vec::new(),
            lines: Arc::new(LineTable::default()),
            unwind: Arc::new(MachoUnwind::default()),
        }
    }

    /// Build [`ImageSymbols`] for a dylib extracted from `dyld_shared_cache`.
    pub(crate) fn from_cached(
        load_addr: u64,
        text_vmaddr: u64,
        text_vmsize: u64,
        module: String,
        unslid_symbols: Vec<(u64, String, Runtime)>,
        unwind: MachoUnwind,
    ) -> Option<Self> {
        let text_end = load_addr.checked_add(text_vmsize)?;
        if text_end <= load_addr {
            return None;
        }
        let slide = load_addr.wrapping_sub(text_vmaddr);
        let mut symbols = Vec::with_capacity(unslid_symbols.len());
        for (unslid, name, lang) in unslid_symbols {
            if !name.is_empty() && unslid != 0 {
                symbols.push((unslid.wrapping_add(slide), name, lang));
            }
        }
        symbols.sort_by_key(|(addr, _, _)| *addr);
        symbols.dedup_by(|right, left| right.0 == left.0 && right.1 == left.1);
        Some(Self {
            start: load_addr,
            end: text_end,
            text_end,
            module,
            rust_image: false,
            go: None,
            slide,
            symbols,
            lines: Arc::new(LineTable::default()),
            unwind: Arc::new(unwind),
        })
    }

    /// Register this image's `__TEXT` range and unwind sections with `unwinder`.
    pub(crate) fn register_unwind_module<U>(&self, unwinder: &mut U)
    where
        U: framehop::Unwinder<Module = framehop::Module<crate::walk::SectionBytes>>,
    {
        if !self.unwind.has_unwind_data() || self.text_end <= self.start {
            return;
        }
        let info = framehop::ExplicitModuleSectionInfo {
            base_svma: self.unwind.base_svma,
            text_svma: self.unwind.text_svma.clone(),
            text: self.unwind.text.clone(),
            stubs_svma: self.unwind.stubs_svma.clone(),
            stub_helper_svma: self.unwind.stub_helper_svma.clone(),
            got_svma: self.unwind.got_svma.clone(),
            unwind_info: self.unwind.unwind_info.clone(),
            eh_frame_svma: self.unwind.eh_frame_svma.clone(),
            eh_frame: self.unwind.eh_frame.clone(),
            ..Default::default()
        };
        unwinder.add_module(framehop::Module::new(
            self.module.clone(),
            self.start..self.text_end,
            self.start,
            info,
        ));
    }

    /// Populate runtime-relocated symbols from unslid Mach-O symbol addresses.
    pub(crate) fn set_unslid_symbols(&mut self, unslid_symbols: Vec<(u64, String, Runtime)>) {
        let mut symbols = Vec::with_capacity(unslid_symbols.len());
        for (unslid, name, lang) in unslid_symbols {
            if !name.is_empty() && unslid != 0 {
                symbols.push((unslid.wrapping_add(self.slide), name, lang));
            }
        }
        symbols.sort_by_key(|(addr, _, _)| *addr);
        symbols.dedup_by(|right, left| right.0 == left.0 && right.1 == left.1);
        self.symbols = symbols;
    }

    /// Parse `bytes` as a thin or universal Mach-O loaded at `load_addr`.
    #[cfg(test)]
    pub(crate) fn parse_bytes(
        bytes: &[u8],
        load_addr: u64,
        module: &str,
        cpu: MachCpu,
    ) -> Option<Self> {
        let cursor = std::io::Cursor::new(bytes);
        parse_reader(cursor, bytes.len() as u64, load_addr, module, cpu, true)
    }

    /// Read segment bounds and unwind sections of `path` without parsing symbols
    /// or DWARF line tables yet.
    pub(crate) fn parse_unwind_file(
        path: &Path,
        load_addr: u64,
        module: &str,
        cpu: MachCpu,
    ) -> Option<Self> {
        let file = std::fs::File::open(path).ok()?;
        let len = file.metadata().ok()?.len();
        parse_reader(file, len, load_addr, module, cpu, false)
    }

    /// Populate symbols and DWARF / sibling `.dSYM` line tables from `path`.
    pub(crate) fn populate_file_symbols(&mut self, path: &Path, cpu: MachCpu) {
        if let Some(full) = Self::parse_file(path, self.start, &self.module, cpu) {
            self.symbols = full.symbols;
            self.lines = full.lines;
        }
    }

    /// Read the symbol table of the binary at `path`.
    ///
    /// `load_addr` is the header's address in the target process. The file
    /// size is not a reason to skip it: only the header, load commands, and
    /// the capped symbol table are read. A dyld shared cache (`dyld_v1`) is
    /// rejected here and handled by [`crate::dyld_cache`].
    pub(crate) fn parse_file(
        path: &Path,
        load_addr: u64,
        module: &str,
        cpu: MachCpu,
    ) -> Option<Self> {
        let file = std::fs::File::open(path).ok()?;
        let len = file.metadata().ok()?.len();
        let mut parsed = parse_reader(file, len, load_addr, module, cpu, true)?;
        if parsed.lines.is_empty()
            && let Some(dsym_lines) = read_dsym_lines(path, cpu)
        {
            parsed.lines = Arc::new(dsym_lines);
        }
        Some(parsed)
    }

    /// Whether `addr` falls inside this image's segments.
    pub(crate) fn contains(&self, addr: u64) -> bool {
        addr >= self.start && addr < self.end
    }

    /// Runtime address of a symbol with this exact name.
    ///
    /// HotSpot exports `gHotSpotVMStructs` as data. [`Self::name_at`] only
    /// answers "what function is this address".
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub(crate) fn address_of(&self, name: &str) -> Option<u64> {
        self.symbols
            .iter()
            .find(|(_, symbol, _)| symbol == name)
            .map(|(addr, _, _)| *addr)
    }

    /// Function name and symbol runtime for `addr`, when a symbol covers it.
    pub(crate) fn name_at(&self, addr: u64) -> Option<(&str, Runtime)> {
        let idx = self.symbols.partition_point(|(start, _, _)| *start <= addr);
        let (start, name, runtime) = self.symbols.get(idx.checked_sub(1)?)?;
        let next = self
            .symbols
            .get(idx)
            .map(|(next, _, _)| *next)
            .unwrap_or(self.end.max(start.saturating_add(MAX_SYMBOL_SPAN)));
        if addr >= next {
            return None;
        }
        if addr.saturating_sub(*start) > MAX_SYMBOL_SPAN {
            return None;
        }
        Some((name.as_str(), *runtime))
    }

    /// Source file path and 1-based line number for `addr`, when `__debug_line` covers it.
    pub(crate) fn location_at(&self, addr: u64) -> Option<(&str, u32)> {
        if !self.contains(addr) {
            return None;
        }
        self.lines.lookup(addr.wrapping_sub(self.slide))
    }
}

/// Preferred `__TEXT` vmaddr and the unslid end of the image's owned address space.
///
/// For dylibs inside `dyld_shared_cache` (`MH_DYLIB_IN_CACHE`), `__LINKEDIT` and
/// `__DATA` sit in shared regions hundreds of MiB past `__TEXT`, so the image's
/// span is bounded to `__TEXT` (`text_vmaddr + text_vmsize`) to prevent cached
/// dylibs from overlapping each other.
pub(crate) fn segment_bounds(header_and_commands: &[u8]) -> Option<(u64, u64)> {
    let header = Header64::read(header_and_commands)?;
    let commands = header_and_commands.get(32..32 + header.sizeofcmds)?;
    let (text_vmaddr, _text_vmsize, end_vmaddr) =
        walk_segments(commands, header.ncmds, header.flags)?;
    Some((text_vmaddr, end_vmaddr))
}

fn parse_reader<R: Read + Seek>(
    mut reader: R,
    len: u64,
    load_addr: u64,
    module: &str,
    cpu: MachCpu,
    include_symbols: bool,
) -> Option<ImageSymbols> {
    let mut magic = [0u8; 8];
    reader.seek(SeekFrom::Start(0)).ok()?;
    if reader.read(&mut magic).ok()? >= 8 && crate::policy::is_dyld_shared_cache(&magic) {
        return None;
    }
    let (offset, size) = select_slice(&mut reader, len, cpu)?;
    if size < 32 || offset.saturating_add(size) > len {
        return None;
    }
    reader.seek(SeekFrom::Start(offset)).ok()?;
    let mut header_buf = [0u8; 32];
    reader.read_exact(&mut header_buf).ok()?;
    let header = Header64::read(&header_buf)?;
    if header.sizeofcmds > 1024 * 1024 {
        return None;
    }
    let mut commands = vec![0u8; header.sizeofcmds];
    reader.read_exact(&mut commands).ok()?;
    let (text_vmaddr, text_vmsize, end_vmaddr) =
        walk_segments(&commands, header.ncmds, header.flags)?;
    let symbols = if include_symbols {
        match find_symtab(&commands, header.ncmds) {
            Some(symtab) => read_symbols(&mut reader, offset, size, &symtab)?,
            None => Vec::new(),
        }
    } else {
        Vec::new()
    };
    let lines = if include_symbols {
        read_dwarf_lines(&mut reader, offset, size, &commands, header.ncmds).unwrap_or_default()
    } else {
        LineTable::default()
    };
    let unwind = read_macho_unwind(
        &mut reader,
        offset,
        size,
        text_vmaddr,
        &commands,
        header.ncmds,
    );
    let slide = load_addr.wrapping_sub(text_vmaddr);
    let mut runtime = Vec::with_capacity(symbols.len());
    for (unslid, name, lang) in symbols {
        let addr = unslid.wrapping_add(slide);
        if !name.is_empty() {
            runtime.push((addr, name, lang));
        }
    }
    runtime.sort_by_key(|(addr, _, _)| *addr);
    let end = end_vmaddr.wrapping_add(slide);
    let text_end = load_addr.saturating_add(text_vmsize);
    if end <= load_addr || text_end <= load_addr {
        return None;
    }
    Some(ImageSymbols {
        start: load_addr,
        end,
        text_end,
        module: module.to_string(),
        rust_image: false,
        go: None,
        slide,
        symbols: runtime,
        lines: Arc::new(lines),
        unwind: Arc::new(unwind),
    })
}

pub(crate) struct Header64 {
    pub ncmds: usize,
    pub sizeofcmds: usize,
    pub flags: u32,
}

impl Header64 {
    pub(crate) fn read(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < 32 || u32::from_le_bytes(bytes[0..4].try_into().ok()?) != MH_MAGIC_64 {
            return None;
        }
        let ncmds = u32::from_le_bytes(bytes[16..20].try_into().ok()?) as usize;
        let sizeofcmds = u32::from_le_bytes(bytes[20..24].try_into().ok()?) as usize;
        let flags = u32::from_le_bytes(bytes[24..28].try_into().ok()?);
        if ncmds > 10_000 {
            return None;
        }
        Some(Self {
            ncmds,
            sizeofcmds,
            flags,
        })
    }
}

pub(crate) struct Symtab {
    pub symoff: u64,
    pub nsyms: u32,
    pub stroff: u64,
    pub strsize: u32,
}

fn select_slice<R: Read + Seek>(reader: &mut R, len: u64, cpu: MachCpu) -> Option<(u64, u64)> {
    let mut magic_buf = [0u8; 4];
    reader.seek(SeekFrom::Start(0)).ok()?;
    reader.read_exact(&mut magic_buf).ok()?;
    let le = u32::from_le_bytes(magic_buf);
    let be = u32::from_be_bytes(magic_buf);
    if le == MH_MAGIC_64 {
        return Some((0, len));
    }
    if be != FAT_MAGIC {
        return None;
    }
    let mut count_buf = [0u8; 4];
    reader.read_exact(&mut count_buf).ok()?;
    let nfat = u32::from_be_bytes(count_buf);
    if nfat == 0 || nfat > 64 {
        return None;
    }
    let want = cpu.cputype();
    for _ in 0..nfat {
        let mut arch = [0u8; 20];
        reader.read_exact(&mut arch).ok()?;
        let cputype = u32::from_be_bytes(arch[0..4].try_into().ok()?);
        let offset = u64::from(u32::from_be_bytes(arch[8..12].try_into().ok()?));
        let size = u64::from(u32::from_be_bytes(arch[12..16].try_into().ok()?));
        if cputype == want {
            return Some((offset, size));
        }
    }
    None
}

/// `__TEXT` `(vmaddr, vmsize)` plus the unslid end of the image's owned segments.
fn walk_segments(commands: &[u8], ncmds: usize, flags: u32) -> Option<(u64, u64, u64)> {
    let mut text_vmaddr = None;
    let mut text_vmsize = 0u64;
    let mut end_vmaddr = 0u64;
    for command in LoadCommands::new(commands, ncmds) {
        // `data` starts at `segname`. A 64-bit segment command is 72 bytes,
        // so 64 bytes remain after `cmd` and `cmdsize`.
        if command.cmd != LC_SEGMENT_64 || command.data.len() < 32 {
            continue;
        }
        let name = segname(&command.data[0..16]);
        let vmaddr = u64::from_le_bytes(command.data[16..24].try_into().ok()?);
        let vmsize = u64::from_le_bytes(command.data[24..32].try_into().ok()?);
        let end = vmaddr.saturating_add(vmsize);
        if end > end_vmaddr {
            end_vmaddr = end;
        }
        if name == "__TEXT" {
            text_vmaddr = Some(vmaddr);
            text_vmsize = vmsize;
        }
    }
    let text_vmaddr = text_vmaddr?;
    if (flags & MH_DYLIB_IN_CACHE) != 0 {
        end_vmaddr = text_vmaddr.saturating_add(text_vmsize);
    }
    if end_vmaddr <= text_vmaddr || text_vmsize == 0 {
        return None;
    }
    Some((text_vmaddr, text_vmsize, end_vmaddr))
}

pub(crate) fn find_symtab(commands: &[u8], ncmds: usize) -> Option<Symtab> {
    for command in LoadCommands::new(commands, ncmds) {
        if command.cmd == LC_SYMTAB && command.data.len() >= 16 {
            return Some(Symtab {
                symoff: u64::from(u32::from_le_bytes(command.data[0..4].try_into().ok()?)),
                nsyms: u32::from_le_bytes(command.data[4..8].try_into().ok()?),
                stroff: u64::from(u32::from_le_bytes(command.data[8..12].try_into().ok()?)),
                strsize: u32::from_le_bytes(command.data[12..16].try_into().ok()?),
            });
        }
    }
    None
}

fn read_symbols<R: Read + Seek>(
    reader: &mut R,
    slice_offset: u64,
    slice_size: u64,
    symtab: &Symtab,
) -> Option<Vec<(u64, String, Runtime)>> {
    if symtab.nsyms > 2_000_000 || symtab.strsize > 64 * 1024 * 1024 {
        return None;
    }
    let sym_bytes = u64::from(symtab.nsyms).checked_mul(16)?;
    let sym_at = slice_offset.checked_add(symtab.symoff)?;
    let str_at = slice_offset.checked_add(symtab.stroff)?;
    if sym_at.checked_add(sym_bytes)? > slice_offset.saturating_add(slice_size) {
        return None;
    }
    if str_at.checked_add(u64::from(symtab.strsize))? > slice_offset.saturating_add(slice_size) {
        return None;
    }
    reader.seek(SeekFrom::Start(str_at)).ok()?;
    let mut strings = vec![0u8; symtab.strsize as usize];
    reader.read_exact(&mut strings).ok()?;
    reader.seek(SeekFrom::Start(sym_at)).ok()?;
    let mut raw = vec![0u8; sym_bytes as usize];
    reader.read_exact(&mut raw).ok()?;
    let mut symbols = Vec::new();
    let (entries, _) = raw.as_chunks::<16>();
    for entry in entries {
        let n_strx = u32::from_le_bytes(entry[0..4].try_into().ok()?) as usize;
        let n_type = entry[4];
        let n_sect = entry[5];
        let n_value = u64::from_le_bytes(entry[8..16].try_into().ok()?);
        if n_sect == 0 || n_value == 0 || n_type & N_STAB != 0 || n_type & N_TYPE != N_SECT {
            continue;
        }
        let Some(raw_name) = cstr_at(&strings, n_strx) else {
            continue;
        };
        let Presented { name, runtime, .. } = symbol::present_macho_symbol(raw_name);
        symbols.push((n_value, name, runtime));
    }
    Some(symbols)
}

fn read_macho_unwind<R: Read + Seek>(
    reader: &mut R,
    slice_offset: u64,
    slice_size: u64,
    base_svma: u64,
    commands: &[u8],
    ncmds: usize,
) -> MachoUnwind {
    let mut unwind = MachoUnwind {
        base_svma,
        ..Default::default()
    };
    let mut text_sec = None;
    let mut unwind_sec = None;
    let mut eh_sec = None;
    for command in LoadCommands::new(commands, ncmds) {
        if command.cmd != LC_SEGMENT_64 || command.data.len() < 64 {
            continue;
        }
        let Ok(nsects_bytes) = command.data[56..60].try_into() else {
            continue;
        };
        let nsects = u32::from_le_bytes(nsects_bytes) as usize;
        let Some(needed) = 64usize.checked_add(nsects.saturating_mul(80)) else {
            continue;
        };
        if command.data.len() < needed {
            continue;
        }
        for idx in 0..nsects {
            let sec = &command.data[64 + idx * 80..64 + (idx + 1) * 80];
            let sect = segname(&sec[0..16]);
            let seg = segname(&sec[16..32]);
            let addr = u64::from_le_bytes(sec[32..40].try_into().unwrap());
            let size = u64::from_le_bytes(sec[40..48].try_into().unwrap());
            let offset = u64::from(u32::from_le_bytes(sec[48..52].try_into().unwrap()));
            let flags = u32::from_le_bytes(sec[64..68].try_into().unwrap());
            if size == 0 || (flags & 0xff) == S_ZEROFILL {
                continue;
            }
            let range = addr..addr.saturating_add(size);
            match (seg, sect) {
                ("__TEXT", "__text") if unwind.text_svma.is_none() => {
                    unwind.text_svma = Some(range);
                    text_sec = Some(MachSection { offset, size });
                }
                ("__TEXT", "__stubs") if unwind.stubs_svma.is_none() => {
                    unwind.stubs_svma = Some(range);
                }
                ("__TEXT", "__stub_helper") if unwind.stub_helper_svma.is_none() => {
                    unwind.stub_helper_svma = Some(range);
                }
                ("__TEXT", "__unwind_info") if unwind_sec.is_none() => {
                    unwind_sec = Some(MachSection { offset, size });
                }
                ("__TEXT", "__eh_frame") if eh_sec.is_none() => {
                    unwind.eh_frame_svma = Some(range);
                    eh_sec = Some(MachSection { offset, size });
                }
                (_, "__got") if unwind.got_svma.is_none() => {
                    unwind.got_svma = Some(range);
                }
                _ => {}
            }
        }
    }
    if let Some(sec) = unwind_sec {
        unwind.unwind_info =
            read_mach_section(reader, slice_offset, slice_size, sec, MAX_UNWIND_INFO_BYTES)
                .map(crate::walk::SectionBytes::new);
    }
    if let Some(sec) = eh_sec {
        unwind.eh_frame =
            read_mach_section(reader, slice_offset, slice_size, sec, MAX_EH_FRAME_BYTES)
                .map(crate::walk::SectionBytes::new);
    }
    if unwind.has_unwind_data()
        && let Some(sec) = text_sec
    {
        unwind.text =
            read_mach_section(reader, slice_offset, slice_size, sec, MAX_TEXT_UNWIND_BYTES)
                .map(crate::walk::SectionBytes::new);
    }
    unwind
}

fn read_dsym_lines(binary_path: &Path, cpu: MachCpu) -> Option<LineTable> {
    let file_name = binary_path.file_name()?;
    let mut bundle = binary_path.as_os_str().to_os_string();
    bundle.push(".dSYM");
    let dwarf_path = Path::new(&bundle)
        .join("Contents")
        .join("Resources")
        .join("DWARF")
        .join(file_name);
    let mut file = std::fs::File::open(dwarf_path).ok()?;
    let len = file.metadata().ok()?.len();
    let (offset, size) = select_slice(&mut file, len, cpu)?;
    if size < 32 || offset.saturating_add(size) > len {
        return None;
    }
    file.seek(SeekFrom::Start(offset)).ok()?;
    let mut header_buf = [0u8; 32];
    file.read_exact(&mut header_buf).ok()?;
    let header = Header64::read(&header_buf)?;
    if header.sizeofcmds > 1024 * 1024 {
        return None;
    }
    let mut commands = vec![0u8; header.sizeofcmds];
    file.read_exact(&mut commands).ok()?;
    read_dwarf_lines(&mut file, offset, size, &commands, header.ncmds)
}

#[derive(Clone, Copy)]
struct MachSection {
    offset: u64,
    size: u64,
}

fn read_dwarf_lines<R: Read + Seek>(
    reader: &mut R,
    slice_offset: u64,
    slice_size: u64,
    commands: &[u8],
    ncmds: usize,
) -> Option<LineTable> {
    let mut line_sec = None;
    let mut line_str_sec = None;
    let mut str_sec = None;
    for command in LoadCommands::new(commands, ncmds) {
        if command.cmd != LC_SEGMENT_64 || command.data.len() < 64 {
            continue;
        }
        let nsects = u32::from_le_bytes(command.data[56..60].try_into().ok()?) as usize;
        let needed = 64usize.checked_add(nsects.checked_mul(80)?)?;
        if command.data.len() < needed {
            continue;
        }
        for idx in 0..nsects {
            let sec = &command.data[64 + idx * 80..64 + (idx + 1) * 80];
            let sect = segname(&sec[0..16]);
            let seg = segname(&sec[16..32]);
            if seg != "__DWARF" {
                continue;
            }
            let size = u64::from_le_bytes(sec[40..48].try_into().ok()?);
            let offset = u64::from(u32::from_le_bytes(sec[48..52].try_into().ok()?));
            let flags = u32::from_le_bytes(sec[64..68].try_into().ok()?);
            if size == 0 || (flags & 0xff) == S_ZEROFILL {
                continue;
            }
            let entry = MachSection { offset, size };
            match sect {
                "__debug_line" if line_sec.is_none() => line_sec = Some(entry),
                "__debug_line_str" if line_str_sec.is_none() => line_str_sec = Some(entry),
                "__debug_str" if str_sec.is_none() => str_sec = Some(entry),
                _ => {}
            }
        }
    }
    let line_sec = line_sec?;
    let debug_line = read_mach_section(
        reader,
        slice_offset,
        slice_size,
        line_sec,
        MAX_LINE_SECTION_BYTES,
    )?;
    let debug_line_str = line_str_sec
        .and_then(|sec| {
            read_mach_section(reader, slice_offset, slice_size, sec, MAX_STR_SECTION_BYTES)
        })
        .unwrap_or_default();
    let debug_str = str_sec
        .and_then(|sec| {
            read_mach_section(reader, slice_offset, slice_size, sec, MAX_STR_SECTION_BYTES)
        })
        .unwrap_or_default();
    let table = LineTable::parse(
        DwarfLineSections {
            debug_line: &debug_line,
            debug_line_str: &debug_line_str,
            debug_str: &debug_str,
        },
        8,
    );
    (!table.is_empty()).then_some(table)
}

fn read_mach_section<R: Read + Seek>(
    reader: &mut R,
    slice_offset: u64,
    slice_size: u64,
    sec: MachSection,
    cap: u64,
) -> Option<Vec<u8>> {
    if sec.size == 0 || sec.size > cap {
        return None;
    }
    let start = slice_offset.checked_add(sec.offset)?;
    let end = start.checked_add(sec.size)?;
    if end > slice_offset.saturating_add(slice_size) {
        return None;
    }
    let mut buf = vec![0u8; sec.size as usize];
    reader.seek(SeekFrom::Start(start)).ok()?;
    reader.read_exact(&mut buf).ok()?;
    Some(buf)
}

fn cstr_at(strings: &[u8], offset: usize) -> Option<&str> {
    let rest = strings.get(offset..)?;
    let end = rest
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(rest.len());
    let bytes = &rest[..end];
    if bytes.is_empty() {
        return None;
    }
    std::str::from_utf8(bytes).ok()
}

pub(crate) fn segname(bytes: &[u8]) -> &str {
    let end = bytes
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(bytes.len());
    std::str::from_utf8(&bytes[..end]).unwrap_or("")
}

pub(crate) struct Command<'a> {
    pub cmd: u32,
    /// Bytes after the 8-byte `cmd` / `cmdsize` pair.
    pub data: &'a [u8],
}

pub(crate) struct LoadCommands<'a> {
    bytes: &'a [u8],
    left: usize,
    cursor: usize,
}

impl<'a> LoadCommands<'a> {
    pub(crate) fn new(bytes: &'a [u8], ncmds: usize) -> Self {
        Self {
            bytes,
            left: ncmds,
            cursor: 0,
        }
    }
}

impl<'a> Iterator for LoadCommands<'a> {
    type Item = Command<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.left == 0 || self.cursor + 8 > self.bytes.len() {
            return None;
        }
        let cmd = u32::from_le_bytes(self.bytes[self.cursor..self.cursor + 4].try_into().ok()?);
        let cmdsize = u32::from_le_bytes(
            self.bytes[self.cursor + 4..self.cursor + 8]
                .try_into()
                .ok()?,
        ) as usize;
        if cmdsize < 8 || self.cursor + cmdsize > self.bytes.len() {
            return None;
        }
        let data = &self.bytes[self.cursor + 8..self.cursor + cmdsize];
        self.cursor += cmdsize;
        self.left -= 1;
        Some(Command { cmd, data })
    }
}

#[cfg(test)]
mod tests {
    use super::{CPU_TYPE_ARM64, FAT_MAGIC, ImageSymbols, LC_SEGMENT_64, LC_SYMTAB, MachCpu};

    fn symbol_name(image: &ImageSymbols, addr: u64) -> Option<&str> {
        image.name_at(addr).map(|(name, _)| name)
    }

    #[test]
    fn itanium_names_are_demangled() {
        let pretty = crate::symbol::present_macho_symbol("__ZN14anyprof_native4spinEv");
        assert_eq!(pretty.runtime, anyprof_core::Runtime::Native);
        assert!(
            pretty.name.contains("anyprof_native") && pretty.name.contains("spin"),
            "{}",
            pretty.name
        );
    }

    #[test]
    fn c_symbols_drop_the_mach_o_underscore() {
        assert_eq!(crate::symbol::present_macho_symbol("_main").name, "main");
        assert_eq!(crate::symbol::present_macho_symbol("main").name, "main");
        assert_eq!(
            crate::symbol::present_macho_symbol("__stack_chk_fail").name,
            "__stack_chk_fail"
        );
    }

    #[test]
    fn lookup_applies_the_aslr_slide_and_stops_at_the_next_symbol() {
        let bytes = synthetic_macho(&[
            ("_anyprof_native_spin", 0x1000_0100),
            ("_main", 0x1000_0200),
        ]);
        let image = ImageSymbols::parse_bytes(&bytes, 0x1040_0000, "busy", MachCpu::Arm64)
            .expect("synthetic image");
        assert_eq!(image.start, 0x1040_0000);
        assert!(image.contains(0x1040_0180));
        assert_eq!(
            symbol_name(&image, 0x1040_0100),
            Some("anyprof_native_spin")
        );
        assert_eq!(
            symbol_name(&image, 0x1040_0180),
            Some("anyprof_native_spin")
        );
        assert_eq!(symbol_name(&image, 0x1040_0200), Some("main"));
        assert_eq!(symbol_name(&image, 0x1040_00f0), None);
        assert!(!image.contains(0x1040_0000 + 0x2000));
    }

    #[test]
    fn a_universal_binary_uses_the_requested_slice() {
        let arm = synthetic_macho(&[("_anyprof_native_spin", 0x1000_0040)]);
        let x64 = synthetic_macho(&[("_other", 0x1000_0040)]);
        let fat = fat_binary(&[(MachCpu::X86_64, &x64), (MachCpu::Arm64, &arm)]);
        let image = ImageSymbols::parse_bytes(&fat, 0x1000_0000, "busy", MachCpu::Arm64).unwrap();
        assert_eq!(
            symbol_name(&image, 0x1000_0040),
            Some("anyprof_native_spin")
        );
        let image = ImageSymbols::parse_bytes(&fat, 0x1000_0000, "busy", MachCpu::X86_64).unwrap();
        assert_eq!(symbol_name(&image, 0x1000_0040), Some("other"));
    }

    #[test]
    fn garbage_and_truncated_images_do_not_parse() {
        assert!(ImageSymbols::parse_bytes(&[], 0x1000, "x", MachCpu::Arm64).is_none());
        assert!(ImageSymbols::parse_bytes(&[0xcf, 0xfa], 0x1000, "x", MachCpu::Arm64).is_none());
        assert!(ImageSymbols::parse_bytes(&[0; 64], 0x1000, "x", MachCpu::Arm64).is_none());
        assert!(
            ImageSymbols::parse_bytes(b"dyld_v1  arm64e\0\0\0", 0x1000, "cache", MachCpu::Arm64)
                .is_none()
        );
    }

    #[test]
    fn a_symbol_count_past_the_cap_is_rejected_when_the_file_is_large_enough() {
        let mut bytes = synthetic_macho(&[("_main", 0x1000_0100)]);
        // nsyms is 12 bytes into the symtab command: 32-byte header, 72-byte
        // segment, then cmd/cmdsize/symoff (12).
        let nsyms_at = 32 + 72 + 12;
        assert_eq!(&bytes[nsyms_at..nsyms_at + 4], &1u32.to_le_bytes());
        let nsyms = 2_000_001u32;
        bytes[nsyms_at..nsyms_at + 4].copy_from_slice(&nsyms.to_le_bytes());
        // symoff is 128. Pad so the claimed nlist range fits in the file.
        // Without the cap the parser would return the real `_main` entry.
        let len = 128 + u64::from(nsyms) * 16;
        assert!(parse_padded("nsyms", &bytes, len).is_none());
    }

    #[test]
    fn a_string_table_past_the_cap_is_rejected_when_the_file_is_large_enough() {
        let mut bytes = synthetic_macho(&[("_main", 0x1000_0100)]);
        // strsize is 20 bytes into the symtab command.
        let strsize_at = 32 + 72 + 20;
        // "_main" plus the leading and trailing NULs.
        assert_eq!(&bytes[strsize_at..strsize_at + 4], &7u32.to_le_bytes());
        let strsize: u32 = 64 * 1024 * 1024 + 1;
        bytes[strsize_at..strsize_at + 4].copy_from_slice(&strsize.to_le_bytes());
        // stroff is 144. The claimed string table fits; the cap must reject it
        // before that buffer is allocated.
        let len = 144 + u64::from(strsize);
        assert!(parse_padded("strsize", &bytes, len).is_none());
    }

    fn parse_padded(label: &str, bytes: &[u8], len: u64) -> Option<ImageSymbols> {
        let dir = std::env::temp_dir().join(format!(
            "anyprof-macho-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("image");
        let mut file = std::fs::File::create(&path).unwrap();
        use std::io::Write;
        file.write_all(bytes).unwrap();
        file.set_len(len).unwrap();
        drop(file);
        let parsed = ImageSymbols::parse_file(&path, 0x1040_0000, label, MachCpu::Arm64);
        let _ = std::fs::remove_dir_all(&dir);
        parsed
    }

    #[test]
    fn a_return_address_on_the_next_symbol_resolves_to_the_caller() {
        let bytes = synthetic_macho(&[("_caller", 0x1000_0100), ("_callee", 0x1000_0200)]);
        let image = ImageSymbols::parse_bytes(&bytes, 0x1040_0000, "busy", MachCpu::Arm64)
            .expect("synthetic image");
        let boundary = 0x1040_0200u64;
        let leaf = crate::policy::symbol_lookup_address(boundary, true);
        let ret = crate::policy::symbol_lookup_address(boundary, false);
        assert_eq!(symbol_name(&image, leaf), Some("callee"));
        assert_eq!(symbol_name(&image, ret), Some("caller"));
    }

    #[test]
    fn a_binary_larger_than_64_mib_still_yields_its_symbol_table() {
        let bytes = synthetic_macho(&[("_anyprof_native_spin", 0x1000_0100)]);
        let dir = std::env::temp_dir().join(format!("anyprof-macho-large-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("huge");
        let mut file = std::fs::File::create(&path).unwrap();
        use std::io::Write;
        file.write_all(&bytes).unwrap();
        file.set_len(64 * 1024 * 1024 + 4096).unwrap();
        drop(file);
        let image = ImageSymbols::parse_file(&path, 0x1040_0000, "huge", MachCpu::Arm64)
            .expect("large file should still parse");
        assert_eq!(
            symbol_name(&image, 0x1040_0100),
            Some("anyprof_native_spin")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sibling_dsym_bundle_resolves_source_file_and_line() {
        let exe_bytes = synthetic_macho(&[("_anyprof_native_spin", 0x1000_0100)]);
        let dsym_bytes = synthetic_dsym_macho(0x1000_0100, 0x40, "/src/busy.cpp", 18);
        let dir = std::env::temp_dir().join(format!("anyprof-macho-dsym-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let dwarf_dir = dir
            .join("busy.dSYM")
            .join("Contents")
            .join("Resources")
            .join("DWARF");
        std::fs::create_dir_all(&dwarf_dir).unwrap();
        let exe_path = dir.join("busy");
        std::fs::write(&exe_path, &exe_bytes).unwrap();
        std::fs::write(dwarf_dir.join("busy"), &dsym_bytes).unwrap();

        let image = ImageSymbols::parse_file(&exe_path, 0x1040_0000, "busy", MachCpu::Arm64)
            .expect("mach-o with dSYM");
        assert_eq!(image.location_at(0x1040_0110), Some(("/src/busy.cpp", 18)));
        assert_eq!(image.location_at(0x1040_0180), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dylib_in_cache_bounds_end_to_text_segment() {
        let mut cmds = Vec::new();
        cmds.extend(segment_text(0x0001_8000_0000, 0x4000));
        let mut linkedit = vec![0u8; 72];
        linkedit[0..4].copy_from_slice(&LC_SEGMENT_64.to_le_bytes());
        linkedit[4..8].copy_from_slice(&72u32.to_le_bytes());
        linkedit[8..18].copy_from_slice(b"__LINKEDIT");
        linkedit[24..32].copy_from_slice(&0x0001_c000_0000u64.to_le_bytes());
        linkedit[32..40].copy_from_slice(&0x0800_0000u64.to_le_bytes());
        cmds.extend(linkedit);

        let mut uncached = vec![0u8; 32];
        uncached[0..4].copy_from_slice(&0xFEED_FACFu32.to_le_bytes());
        uncached[4..8].copy_from_slice(&CPU_TYPE_ARM64.to_le_bytes());
        uncached[12..16].copy_from_slice(&6u32.to_le_bytes()); // MH_DYLIB
        uncached[16..20].copy_from_slice(&2u32.to_le_bytes());
        uncached[20..24].copy_from_slice(&(cmds.len() as u32).to_le_bytes());
        let mut uncached_full = uncached.clone();
        uncached_full.extend_from_slice(&cmds);
        assert_eq!(
            super::segment_bounds(&uncached_full),
            Some((0x0001_8000_0000, 0x0001_c000_0000 + 0x0800_0000))
        );

        let mut cached = uncached;
        cached[24..28].copy_from_slice(&super::MH_DYLIB_IN_CACHE.to_le_bytes());
        cached.extend_from_slice(&cmds);
        assert_eq!(
            super::segment_bounds(&cached),
            Some((0x0001_8000_0000, 0x0001_8000_0000 + 0x4000))
        );
    }

    fn synthetic_dsym_macho(addr: u64, len: u8, path: &str, line: u8) -> Vec<u8> {
        let mut hdr = vec![1, 1, 1, (-5i8) as u8, 14, 13];
        hdr.extend_from_slice(&[0, 1, 1, 1, 1, 0, 0, 0, 1, 0, 0, 1]);
        hdr.push(0); // empty include_directories
        hdr.extend_from_slice(path.as_bytes());
        hdr.extend_from_slice(&[0, 0, 0, 0, 0]);

        let mut prog = vec![0, 9, 2]; // DW_LNE_SET_ADDRESS
        prog.extend_from_slice(&addr.to_le_bytes());
        prog.extend_from_slice(&[3, line.saturating_sub(1), 1]); // DW_LNS_ADVANCE_LINE, DW_LNS_COPY
        prog.extend_from_slice(&[2, len, 0, 1, 1]); // DW_LNS_ADVANCE_PC, DW_LNE_END_SEQUENCE

        let mut unit = Vec::new();
        unit.extend_from_slice(&4u16.to_le_bytes());
        unit.extend_from_slice(&(hdr.len() as u32).to_le_bytes());
        unit.extend_from_slice(&hdr);
        unit.extend_from_slice(&prog);

        let mut debug_line = Vec::new();
        debug_line.extend_from_slice(&(unit.len() as u32).to_le_bytes());
        debug_line.extend_from_slice(&unit);

        let seg_cmd_len = 72 + 80;
        let sec_offset = 32 + seg_cmd_len;
        let mut seg = vec![0u8; seg_cmd_len];
        seg[0..4].copy_from_slice(&LC_SEGMENT_64.to_le_bytes());
        seg[4..8].copy_from_slice(&(seg_cmd_len as u32).to_le_bytes());
        seg[8..15].copy_from_slice(b"__DWARF");
        seg[64..68].copy_from_slice(&1u32.to_le_bytes()); // nsects = 1
        let sec = &mut seg[72..152];
        sec[0..12].copy_from_slice(b"__debug_line");
        sec[16..23].copy_from_slice(b"__DWARF");
        sec[40..48].copy_from_slice(&(debug_line.len() as u64).to_le_bytes());
        sec[48..52].copy_from_slice(&(sec_offset as u32).to_le_bytes());

        let mut header = [0u8; 32];
        header[0..4].copy_from_slice(&0xFEED_FACFu32.to_le_bytes());
        header[4..8].copy_from_slice(&CPU_TYPE_ARM64.to_le_bytes());
        header[12..16].copy_from_slice(&0x0au32.to_le_bytes()); // MH_DSYM
        header[16..20].copy_from_slice(&1u32.to_le_bytes()); // ncmds = 1
        header[20..24].copy_from_slice(&(seg_cmd_len as u32).to_le_bytes());

        let mut out = Vec::new();
        out.extend_from_slice(&header);
        out.extend_from_slice(&seg);
        out.extend_from_slice(&debug_line);
        out
    }

    fn synthetic_macho(symbols: &[(&str, u64)]) -> Vec<u8> {
        let mut strings = vec![0u8];
        let mut nlists = Vec::new();
        for (name, addr) in symbols {
            let n_strx = strings.len() as u32;
            strings.extend(name.as_bytes());
            strings.push(0);
            let mut entry = [0u8; 16];
            entry[0..4].copy_from_slice(&n_strx.to_le_bytes());
            entry[4] = 0x0f; // N_SECT | N_EXT
            entry[5] = 1;
            entry[8..16].copy_from_slice(&addr.to_le_bytes());
            nlists.extend(entry);
        }
        let header_and_cmds = 32 + 72 + 24;
        let symoff = header_and_cmds;
        let stroff = symoff + nlists.len();
        let mut commands = Vec::new();
        commands.extend(segment_text(0x1000_0000, 0x2000));
        commands.extend(symtab_command(
            symoff as u32,
            symbols.len() as u32,
            stroff as u32,
            strings.len() as u32,
        ));
        let mut bytes = Vec::new();
        bytes.extend(mach_header(commands.len() as u32));
        bytes.extend(commands);
        assert_eq!(bytes.len(), header_and_cmds);
        bytes.extend(nlists);
        bytes.extend(strings);
        bytes
    }

    fn mach_header(sizeofcmds: u32) -> [u8; 32] {
        let mut header = [0u8; 32];
        header[0..4].copy_from_slice(&0xFEED_FACFu32.to_le_bytes());
        header[4..8].copy_from_slice(&CPU_TYPE_ARM64.to_le_bytes());
        header[12..16].copy_from_slice(&2u32.to_le_bytes()); // MH_EXECUTE
        header[16..20].copy_from_slice(&2u32.to_le_bytes()); // ncmds
        header[20..24].copy_from_slice(&sizeofcmds.to_le_bytes());
        header
    }

    fn segment_text(vmaddr: u64, vmsize: u64) -> Vec<u8> {
        let mut cmd = vec![0u8; 72];
        cmd[0..4].copy_from_slice(&LC_SEGMENT_64.to_le_bytes());
        cmd[4..8].copy_from_slice(&72u32.to_le_bytes());
        cmd[8..14].copy_from_slice(b"__TEXT");
        cmd[24..32].copy_from_slice(&vmaddr.to_le_bytes());
        cmd[32..40].copy_from_slice(&vmsize.to_le_bytes());
        cmd
    }

    fn symtab_command(symoff: u32, nsyms: u32, stroff: u32, strsize: u32) -> Vec<u8> {
        let mut cmd = vec![0u8; 24];
        cmd[0..4].copy_from_slice(&LC_SYMTAB.to_le_bytes());
        cmd[4..8].copy_from_slice(&24u32.to_le_bytes());
        cmd[8..12].copy_from_slice(&symoff.to_le_bytes());
        cmd[12..16].copy_from_slice(&nsyms.to_le_bytes());
        cmd[16..20].copy_from_slice(&stroff.to_le_bytes());
        cmd[20..24].copy_from_slice(&strsize.to_le_bytes());
        cmd
    }

    fn fat_binary(slices: &[(MachCpu, &[u8])]) -> Vec<u8> {
        let header = 8 + slices.len() * 20;
        let mut bytes = vec![0u8; header];
        bytes[0..4].copy_from_slice(&FAT_MAGIC.to_be_bytes());
        bytes[4..8].copy_from_slice(&(slices.len() as u32).to_be_bytes());
        let mut cursor = header;
        for (index, (cpu, slice)) in slices.iter().enumerate() {
            let at = 8 + index * 20;
            bytes[at..at + 4].copy_from_slice(&cpu.cputype().to_be_bytes());
            let offset = cursor as u32;
            let size = slice.len() as u32;
            bytes[at + 8..at + 12].copy_from_slice(&offset.to_be_bytes());
            bytes[at + 12..at + 16].copy_from_slice(&size.to_be_bytes());
            cursor += slice.len();
        }
        for (_, slice) in slices {
            bytes.extend_from_slice(slice);
        }
        bytes
    }
}
