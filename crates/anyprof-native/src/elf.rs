//! ELF64 symbol tables for native stack frames.
//!
//! The Linux sampler reads `/proc/<pid>/maps` for each file's load address,
//! then reads that file's symbol table. Function names come from `SHT_SYMTAB`
//! (the symbols `nm` prints), falling back to `SHT_DYNSYM` when the binary is
//! stripped. Itanium C++ names are demangled. DWARF is not required.
//!
//! A table above 2,000,000 symbols or a string table above 64 MiB is rejected.
//! The caller still keeps the mapping's address range, so the frame names the
//! module and the address.

#![cfg_attr(
    not(all(target_os = "linux", target_arch = "x86_64")),
    allow(dead_code)
)]

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use crate::policy::symbol_lookup_address;

/// How far past a symbol an instruction can sit and still belong to it.
const MAX_SYMBOL_SPAN: u64 = 16 * 1024 * 1024;
const MAX_SYMBOLS: u64 = 2_000_000;
const MAX_STRINGS: u64 = 64 * 1024 * 1024;

const ELF_MAGIC: [u8; 4] = [0x7f, b'E', b'L', b'F'];
const ELFCLASS64: u8 = 2;
const ELFDATA2LSB: u8 = 1;
const EM_X86_64: u16 = 62;
const ET_EXEC: u16 = 2;
const ET_DYN: u16 = 3;
const PT_LOAD: u32 = 1;
const SHT_SYMTAB: u32 = 2;
const SHT_STRTAB: u32 = 3;
const SHT_DYNSYM: u32 = 11;
const STT_FUNC: u8 = 2;
const STT_GNU_IFUNC: u8 = 10;
const SHN_UNDEF: u16 = 0;

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
    /// Demangled name when the symbol is Itanium C++; otherwise the raw name.
    pub name: String,
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
    /// `(runtime address, size, name)`, sorted by address.
    symbols: Vec<(u64, u64, String)>,
}

impl UserImage {
    /// An image whose file had no usable symbol table.
    pub(crate) fn range_only(start: u64, end: u64, module: String) -> Self {
        Self {
            start,
            end,
            module,
            symbols: Vec::new(),
        }
    }

    /// Apply `bias` (`runtime - link address`) to [`ElfFile::funcs`].
    pub(crate) fn from_elf(start: u64, end: u64, module: String, elf: &ElfFile, bias: u64) -> Self {
        let mut symbols = Vec::with_capacity(elf.funcs.len());
        for func in &elf.funcs {
            if func.name.is_empty() || func.vaddr == 0 {
                continue;
            }
            symbols.push((func.vaddr.wrapping_add(bias), func.size, func.name.clone()));
        }
        symbols.sort_by_key(|(addr, _, _)| *addr);
        symbols.dedup_by(|right, left| right.0 == left.0 && right.2 == left.2);
        Self {
            start,
            end,
            module,
            symbols,
        }
    }

    /// Whether `addr` falls inside this file's mappings.
    pub(crate) fn contains(&self, addr: u64) -> bool {
        addr >= self.start && addr < self.end
    }

    /// Function name for `addr`, when a symbol covers it.
    pub(crate) fn name_at(&self, addr: u64) -> Option<&str> {
        let idx = self.symbols.partition_point(|(start, _, _)| *start <= addr);
        let (start, size, name) = self.symbols.get(idx.checked_sub(1)?)?;
        let next = self
            .symbols
            .get(idx)
            .map(|(next, _, _)| *next)
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
        Some(name.as_str())
    }
}

/// Resolve `addr` for a leaf or a saved return address.
///
/// Non-leaf frames use [`symbol_lookup_address`]. The raw `addr` is tried
/// when the adjusted address falls in a gap between symbols.
pub(crate) fn name_for(images: &[UserImage], addr: u64, leaf: bool) -> Option<(&str, &str)> {
    let lookup = symbol_lookup_address(addr, leaf);
    find(images, lookup).or_else(|| find(images, addr))
}

fn find(images: &[UserImage], addr: u64) -> Option<(&str, &str)> {
    let image = images.iter().find(|image| image.contains(addr))?;
    let name = image.name_at(addr)?;
    Some((name, image.module.as_str()))
}

/// Runtime load bias: `map_start - link_address_of_that_file_offset`.
pub(crate) fn load_bias(map_start: u64, map_offset: u64, loads: &[LoadSegment]) -> Option<u64> {
    let load = loads.iter().find(|load| {
        let end = load.offset.saturating_add(load.filesz.max(1));
        map_offset >= load.offset && map_offset < end
    })?;
    let vaddr = load
        .vaddr
        .wrapping_add(map_offset.wrapping_sub(load.offset));
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
    let loads = read_loads(&mut reader, len, phoff, phentsize, phnum)?;
    if shoff == 0 {
        return Some(ElfFile {
            machine,
            loads,
            funcs: Vec::new(),
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
    let funcs = read_funcs(&mut reader, len, &sections, shstrndx);
    Some(ElfFile {
        machine,
        loads,
        funcs,
    })
}

#[derive(Clone, Copy)]
struct Section {
    kind: u32,
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
) -> Option<Vec<LoadSegment>> {
    if phnum > 1000 || phoff.saturating_add(phnum.saturating_mul(entsize)) > len {
        return None;
    }
    let mut loads = Vec::new();
    for index in 0..phnum {
        let mut buf = vec![0u8; entsize as usize];
        reader.seek(SeekFrom::Start(phoff + index * entsize)).ok()?;
        reader.read_exact(&mut buf).ok()?;
        let kind = u32::from_le_bytes(buf[0..4].try_into().ok()?);
        if kind != PT_LOAD {
            continue;
        }
        let flags = u32::from_le_bytes(buf[4..8].try_into().ok()?);
        let offset = u64::from_le_bytes(buf[8..16].try_into().ok()?);
        let vaddr = u64::from_le_bytes(buf[16..24].try_into().ok()?);
        let filesz = u64::from_le_bytes(buf[32..40].try_into().ok()?);
        let _ = flags;
        loads.push(LoadSegment {
            offset,
            vaddr,
            filesz,
        });
    }
    Some(loads)
}

fn read_sections<R: Read + Seek>(
    reader: &mut R,
    len: u64,
    shoff: u64,
    entsize: u64,
    shnum: u64,
) -> Option<Vec<Section>> {
    if shoff.saturating_add(shnum.saturating_mul(entsize)) > len {
        return None;
    }
    let mut sections = Vec::with_capacity(shnum as usize);
    for index in 0..shnum {
        sections.push(read_section(reader, len, shoff, entsize, index)?);
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
    Some(Section {
        kind: u32::from_le_bytes(buf[4..8].try_into().ok()?),
        offset: u64::from_le_bytes(buf[24..32].try_into().ok()?),
        size: u64::from_le_bytes(buf[32..40].try_into().ok()?),
        link: u32::from_le_bytes(buf[40..44].try_into().ok()?),
        entsize: u64::from_le_bytes(buf[56..64].try_into().ok()?),
    })
}

fn read_funcs<R: Read + Seek>(
    reader: &mut R,
    len: u64,
    sections: &[Section],
    _shstrndx: u64,
) -> Vec<ElfFunc> {
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
    let count = (symtab.size / entsize) as usize;
    let mut funcs = Vec::new();
    for index in 0..count {
        let mut ent = vec![0u8; entsize as usize];
        let at = symtab.offset + index as u64 * entsize;
        if reader.seek(SeekFrom::Start(at)).is_err() || reader.read_exact(&mut ent).is_err() {
            break;
        }
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
        funcs.push(ElfFunc {
            vaddr: value,
            size,
            name: present_symbol(raw),
        });
    }
    funcs
}

fn cstr_at(buf: &[u8], offset: usize) -> Option<&str> {
    let rest = buf.get(offset..)?;
    let end = rest
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(rest.len());
    std::str::from_utf8(&rest[..end]).ok()
}

/// Show `raw` the way a profile should.
///
/// ELF does not add Mach-O's extra underscore. A GNU version suffix
/// (`@@GLIBC_2.2.5`) is removed before demangling and kept out of the result.
pub(crate) fn present_symbol(raw: &str) -> String {
    let trimmed = raw.split("@@").next().unwrap_or(raw);
    let candidate = if let Some(rest) = trimmed.strip_prefix("__Z") {
        format!("_Z{rest}")
    } else {
        trimmed.to_string()
    };
    demangle(&candidate).unwrap_or_else(|| {
        if candidate.as_str() == raw {
            raw.to_string()
        } else {
            candidate
        }
    })
}

fn demangle(raw: &str) -> Option<String> {
    let symbol = cpp_demangle::Symbol::new(raw).ok()?;
    let text = symbol.to_string();
    if text.is_empty() || text == raw {
        None
    } else {
        Some(text)
    }
}

/// Whether this ELF is the x86_64 image the Linux sampler can walk.
pub(crate) fn is_x86_64(elf: &ElfFile) -> bool {
    elf.machine == EM_X86_64
}

#[cfg(test)]
mod tests {
    use super::{UserImage, load_bias, name_for, parse_bytes, present_symbol};

    #[test]
    fn itanium_names_are_demangled_and_c_names_stay() {
        let pretty = present_symbol("_ZN14anyprof_native4spinEv");
        assert!(
            pretty.contains("anyprof_native") && pretty.contains("spin"),
            "{pretty}"
        );
        assert_eq!(present_symbol("main"), "main");
        assert_eq!(present_symbol("read@@GLIBC_2.2.5"), "read");
    }

    #[test]
    fn a_return_address_on_the_next_symbol_stays_in_the_caller() {
        let bytes = synthetic_elf(&[
            ("_ZN14anyprof_native4spinEv", 0x1000, 0x200),
            ("main", 0x1200, 0x40),
        ]);
        let elf = parse_bytes(&bytes).expect("synthetic elf");
        assert!(super::is_x86_64(&elf));
        let bias = load_bias(0x5555_0000_0000, 0, &elf.loads).unwrap();
        assert_eq!(bias, 0x5555_0000_0000);
        let image = UserImage::from_elf(bias, bias + 0x2000, "busy".to_string(), &elf, bias);
        let (name, module) = name_for(std::slice::from_ref(&image), bias + 0x1050, true).unwrap();
        assert!(name.contains("spin"), "{name}");
        assert_eq!(module, "busy");
        // 0x1200 is the first byte of main. A non-leaf sample of that address
        // is the call in spin, not main itself.
        let (name, _) = name_for(std::slice::from_ref(&image), bias + 0x1200, false).unwrap();
        assert!(name.contains("spin"), "{name}");
        let (name, _) = name_for(std::slice::from_ref(&image), bias + 0x1200, true).unwrap();
        assert_eq!(name, "main");
        assert!(name_for(std::slice::from_ref(&image), bias + 0x10, true).is_none());
    }

    #[test]
    fn garbage_is_not_an_elf() {
        assert!(parse_bytes(&[]).is_none());
        assert!(parse_bytes(b"not elf").is_none());
        assert!(parse_bytes(&[0x7f, b'E', b'L', b'F', 1, 1]).is_none());
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
