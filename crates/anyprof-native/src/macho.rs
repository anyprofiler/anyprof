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
//! of string table. A larger on-disk binary is still parsed. A `dyld_v1`
//! shared cache is not a Mach-O image: system libraries that live only in
//! that cache keep an address range from the in-memory header and do not get
//! function names.

#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use anyprof_core::Runtime;

use crate::symbol::{self, Presented};

/// How far past a symbol an instruction can sit and still belong to it.
///
/// A gap larger than this is padding or an unmapped hole, not the function.
const MAX_SYMBOL_SPAN: u64 = 16 * 1024 * 1024;

const MH_MAGIC_64: u32 = 0xFEED_FACF;
const FAT_MAGIC: u32 = 0xCAFE_BABE;
const CPU_TYPE_X86_64: u32 = 0x0100_0007;
const CPU_TYPE_ARM64: u32 = 0x0100_000C;
const LC_SYMTAB: u32 = 0x2;
const LC_SEGMENT_64: u32 = 0x19;
const N_STAB: u8 = 0xe0;
const N_TYPE: u8 = 0x0e;
const N_SECT: u8 = 0x0e;

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
    /// File name (`busy`, `libsystem_kernel.dylib`, ...).
    pub module: String,
    /// The file contains a Rust image marker.
    pub(crate) rust_image: bool,
    /// `(runtime address, display name, symbol runtime)`, sorted by address.
    symbols: Vec<(u64, String, Runtime)>,
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
            module,
            rust_image: false,
            symbols: Vec::new(),
        }
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
        parse_reader(cursor, bytes.len() as u64, load_addr, module, cpu)
    }

    /// Read the symbol table of the binary at `path`.
    ///
    /// `load_addr` is the header's address in the target process. The file
    /// size is not a reason to skip it: only the header, load commands, and
    /// the capped symbol table are read. A dyld shared cache (`dyld_v1`) is
    /// rejected. The caller still keeps the in-memory segment range when this
    /// returns `None`.
    pub(crate) fn parse_file(
        path: &Path,
        load_addr: u64,
        module: &str,
        cpu: MachCpu,
    ) -> Option<Self> {
        let file = std::fs::File::open(path).ok()?;
        let len = file.metadata().ok()?.len();
        parse_reader(file, len, load_addr, module, cpu)
    }

    /// Whether `addr` falls inside this image's segments.
    pub(crate) fn contains(&self, addr: u64) -> bool {
        addr >= self.start && addr < self.end
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
}

/// Preferred `__TEXT` vmaddr and the unslid end of the last segment.
///
/// The sampler reads this from the target's memory so an image still has a
/// range when its file has no symbol table (dyld shared-cache stubs).
pub(crate) fn segment_bounds(header_and_commands: &[u8]) -> Option<(u64, u64)> {
    let header = Header64::read(header_and_commands)?;
    let commands = header_and_commands.get(32..32 + header.sizeofcmds)?;
    let (text_vmaddr, end_vmaddr) = walk_segments(commands, header.ncmds)?;
    Some((text_vmaddr, end_vmaddr))
}

fn parse_reader<R: Read + Seek>(
    mut reader: R,
    len: u64,
    load_addr: u64,
    module: &str,
    cpu: MachCpu,
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
    let (text_vmaddr, end_vmaddr) = walk_segments(&commands, header.ncmds)?;
    let symtab = find_symtab(&commands, header.ncmds)?;
    let symbols = read_symbols(&mut reader, offset, size, &symtab)?;
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
    if end <= load_addr {
        return None;
    }
    Some(ImageSymbols {
        start: load_addr,
        end,
        module: module.to_string(),
        rust_image: false,
        symbols: runtime,
    })
}

struct Header64 {
    ncmds: usize,
    sizeofcmds: usize,
}

impl Header64 {
    fn read(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < 32 || u32::from_le_bytes(bytes[0..4].try_into().ok()?) != MH_MAGIC_64 {
            return None;
        }
        let ncmds = u32::from_le_bytes(bytes[16..20].try_into().ok()?) as usize;
        let sizeofcmds = u32::from_le_bytes(bytes[20..24].try_into().ok()?) as usize;
        if ncmds > 10_000 {
            return None;
        }
        Some(Self { ncmds, sizeofcmds })
    }
}

struct Symtab {
    symoff: u64,
    nsyms: u32,
    stroff: u64,
    strsize: u32,
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

/// `__TEXT` vmaddr plus the highest `vmaddr + vmsize` across 64-bit segments.
fn walk_segments(commands: &[u8], ncmds: usize) -> Option<(u64, u64)> {
    let mut text_vmaddr = None;
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
        }
    }
    let text_vmaddr = text_vmaddr?;
    if end_vmaddr <= text_vmaddr {
        return None;
    }
    Some((text_vmaddr, end_vmaddr))
}

fn find_symtab(commands: &[u8], ncmds: usize) -> Option<Symtab> {
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

fn segname(bytes: &[u8]) -> &str {
    let end = bytes
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(bytes.len());
    std::str::from_utf8(&bytes[..end]).unwrap_or("")
}

struct Command<'a> {
    cmd: u32,
    /// Bytes after the 8-byte `cmd` / `cmdsize` pair.
    data: &'a [u8],
}

struct LoadCommands<'a> {
    bytes: &'a [u8],
    left: usize,
    cursor: usize,
}

impl<'a> LoadCommands<'a> {
    fn new(bytes: &'a [u8], ncmds: usize) -> Self {
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
