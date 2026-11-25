//! Shared ELF64, Mach-O 64, and PE32+/PE32 header, section, and export readers
//! built on `object::ReadCache` so individual samplers and runtime probes do
//! not duplicate binary offset arithmetic.

use object::endian::{LittleEndian, U16, U32};
use object::read::elf::{FileHeader as _, ProgramHeader as _, SectionHeader as _, Sym as _};
use object::read::macho::{FatArch as _, LoadCommandIterator, MachHeader as _};
use object::read::pe::ImageNtHeaders as _;
use object::read::{ReadRef, SectionIndex};
use object::{Bytes, elf, macho, pe, pod};

use crate::elf::LoadSegment;

const LE: LittleEndian = LittleEndian;

/// Parsed 64-bit little-endian ELF header, `PT_LOAD` segments, and section table.
pub(crate) struct Elf64<'data, R: ReadRef<'data>> {
    pub header: &'data elf::FileHeader64<LittleEndian>,
    pub loads: Vec<LoadSegment>,
    pub gnu_eh_frame: Option<LoadSegment>,
    pub sections: object::read::elf::SectionTable<'data, elf::FileHeader64<LittleEndian>, R>,
}

/// Parse a 64-bit little-endian ELF object from `data` with bounded program and
/// section header counts.
pub(crate) fn parse_elf64<'data, R: ReadRef<'data>>(
    data: R,
    max_sections: usize,
) -> Option<Elf64<'data, R>> {
    let header = elf::FileHeader64::<LittleEndian>::parse(data).ok()?;
    if !header.is_little_endian() {
        return None;
    }
    let phnum = header.phnum(LE, data).ok()?;
    if phnum > 1000 {
        return None;
    }
    let mut loads = Vec::new();
    let mut gnu_eh_frame = None;
    if header.e_phoff(LE) != 0 && phnum > 0 {
        let phdrs = header.program_headers(LE, data).ok()?;
        for ph in phdrs {
            let kind = ph.p_type(LE);
            if kind != elf::PT_LOAD && kind != elf::PT_GNU_EH_FRAME {
                continue;
            }
            let seg = LoadSegment {
                offset: ph.p_offset(LE),
                vaddr: ph.p_vaddr(LE),
                filesz: ph.p_filesz(LE),
            };
            if kind == elf::PT_LOAD {
                loads.push(seg);
            } else if gnu_eh_frame.is_none() && seg.filesz > 0 {
                gnu_eh_frame = Some(seg);
            }
        }
    }
    let shnum = header.shnum(LE, data).ok()? as usize;
    if shnum > max_sections {
        return None;
    }
    let raw_sections = header.section_headers(LE, data).ok()?;
    let strings = header
        .shstrndx(LE, data)
        .ok()
        .and_then(|idx| raw_sections.get(idx as usize))
        .and_then(|sec| read_elf64_section(data, sec, 64 * 1024 * 1024))
        .map(object::read::StringTable::new)
        .unwrap_or_default();
    let sections = object::read::elf::SectionTable::new(raw_sections, strings);
    Some(Elf64 {
        header,
        loads,
        gnu_eh_frame,
        sections,
    })
}

/// Visit every symbol in matching `section_type` (`SHT_SYMTAB` or `SHT_DYNSYM`)
/// sections whose entry count and string table size fit within `max_symbols`
/// and `max_strings`.
pub(crate) fn visit_elf64_symbols<'data, R: ReadRef<'data>>(
    data: R,
    sections: &object::read::elf::SectionTable<'data, elf::FileHeader64<LittleEndian>, R>,
    section_type: elf::SectionType,
    max_symbols: usize,
    max_strings: u64,
    mut visit: impl FnMut(&'data str, &'data elf::Sym64<LittleEndian>),
) -> bool {
    let mut visited_table = false;
    for (_index, section) in sections.enumerate() {
        if section.sh_type(LE) != section_type {
            continue;
        }
        let entsize = if section.sh_entsize(LE) == 0 {
            24
        } else {
            section.sh_entsize(LE)
        };
        let size = section.sh_size(LE);
        if entsize < 24 || size == 0 || (size / entsize) > max_symbols as u64 {
            continue;
        }
        let link = SectionIndex(section.sh_link(LE) as usize);
        let Ok(str_sec) = sections.section(link) else {
            continue;
        };
        let str_size = str_sec.sh_size(LE);
        if str_size == 0 || str_size > max_strings {
            continue;
        }
        // Read the linked string table directly so synthetic test fixtures that
        // leave `sh_type == 0` on `.strtab` still resolve identically to real
        // `SHT_STRTAB` sections.
        let Some((str_off, str_len)) = str_sec.file_range(LE) else {
            continue;
        };
        let Ok(str_bytes) = data.read_bytes_at(str_off, str_len) else {
            continue;
        };
        let Some((sym_off, sym_len)) = section.file_range(LE) else {
            continue;
        };
        let Ok(sym_bytes) = data.read_bytes_at(sym_off, sym_len) else {
            continue;
        };
        let step = entsize as usize;
        let count = sym_bytes.len() / step;
        visited_table = true;
        for i in 0..count {
            let Some(chunk) = sym_bytes.get(i * step..i * step + 24) else {
                break;
            };
            let Ok((sym, _)) = pod::from_bytes::<elf::Sym64<LittleEndian>>(chunk) else {
                continue;
            };
            let name_off = sym.st_name(LE) as usize;
            let Some(rest) = str_bytes.get(name_off..) else {
                continue;
            };
            let end = rest.iter().position(|&b| b == 0).unwrap_or(rest.len());
            let Ok(name) = std::str::from_utf8(&rest[..end]) else {
                continue;
            };
            visit(name, sym);
        }
    }
    visited_table
}

/// Read an uncompressed, file-backed ELF64 section whose size is `1..=cap`.
pub(crate) fn read_elf64_section<'data, R: ReadRef<'data>>(
    data: R,
    section: &elf::SectionHeader64<LittleEndian>,
    cap: u64,
) -> Option<&'data [u8]> {
    if section.sh_type(LE) == elf::SHT_NOBITS || section.sh_flags(LE).contains(elf::SHF_COMPRESSED)
    {
        return None;
    }
    let (offset, size) = section.file_range(LE)?;
    if size == 0 || size > cap {
        return None;
    }
    data.read_bytes_at(offset, size).ok()
}

/// Host Mach-O `cputype` (`CPU_TYPE_ARM64` or `CPU_TYPE_X86_64`).
pub(crate) const fn macho_host_cputype() -> u32 {
    #[cfg(target_arch = "aarch64")]
    {
        macho::CPU_TYPE_ARM64.0
    }
    #[cfg(not(target_arch = "aarch64"))]
    {
        macho::CPU_TYPE_X86_64.0
    }
}

/// Select the `(offset, size)` slice of a thin 64-bit Mach-O or 32-bit universal
/// (`FAT_MAGIC`) binary matching `want_cputype`.
pub(crate) fn macho_slice<'data, R: ReadRef<'data>>(
    data: R,
    len: u64,
    want_cputype: u32,
) -> Option<(u64, u64)> {
    if macho::MachHeader64::<LittleEndian>::parse(data, 0).is_ok() {
        return Some((0, len));
    }
    let fat_hdr = data.read_at::<macho::FatHeader>(0).ok()?;
    if fat_hdr.magic.get(object::BigEndian) != macho::FAT_MAGIC {
        return None;
    }
    let nfat = fat_hdr.nfat_arch.get(object::BigEndian);
    if !(1..=64).contains(&nfat) {
        return None;
    }
    let fat = object::read::macho::MachOFatFile32::parse(data).ok()?;
    for arch in fat.arches() {
        if arch.cputype().0 == want_cputype {
            let (offset, size) = arch.file_range();
            if offset.checked_add(size)? <= len {
                return Some((offset, size));
            }
            return None;
        }
    }
    None
}

/// Parse a 64-bit little-endian Mach-O header and bounded load-command iterator
/// at offset `0` of `slice_data`.
pub(crate) fn macho_commands<'data, R: ReadRef<'data>>(
    slice_data: R,
    max_sizeofcmds: u32,
) -> Option<(
    &'data macho::MachHeader64<LittleEndian>,
    LoadCommandIterator<'data, LittleEndian>,
)> {
    let header = macho::MachHeader64::<LittleEndian>::parse(slice_data, 0).ok()?;
    if !header.is_little_endian() {
        return None;
    }
    let ncmds = header.ncmds(LE);
    let sizeofcmds = header.sizeofcmds(LE);
    if ncmds > 10_000 || sizeofcmds > max_sizeofcmds {
        return None;
    }
    let commands = header.load_commands(LE, slice_data, 0).ok()?;
    Some((header, commands))
}

/// NUL-padded 16-byte Mach-O segment or section name.
pub(crate) fn macho_name(bytes: &[u8; 16]) -> &str {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    std::str::from_utf8(&bytes[..end]).unwrap_or("")
}

/// Parsed PE32+ (64-bit) headers, data directories, and section table.
#[allow(dead_code)]
pub(crate) struct Pe64<'data> {
    pub machine: u16,
    pub image_base: u64,
    pub data_dirs: object::read::pe::DataDirectories<'data>,
    pub sections: object::read::pe::SectionTable<'data>,
}

/// Error returned when validating a PE32+ image.
#[allow(dead_code)]
pub(crate) enum Pe64Error {
    InvalidPe,
    NotPe32Plus,
}

/// Parse a PE32+ image's DOS/NT headers, data directories, and section table.
pub(crate) fn parse_pe64<'data, R: ReadRef<'data>>(
    data: R,
    max_sections: usize,
) -> Result<Pe64<'data>, Pe64Error> {
    let dos = pe::ImageDosHeader::parse(data).map_err(|_| Pe64Error::InvalidPe)?;
    let nt_off = u64::from(dos.nt_headers_offset());
    if nt_off > 1024 * 1024 {
        return Err(Pe64Error::InvalidPe);
    }
    let magic = object::read::pe::optional_header_magic(data).map_err(|_| Pe64Error::InvalidPe)?;
    if magic != pe::IMAGE_NT_OPTIONAL_HDR64_MAGIC {
        return Err(Pe64Error::NotPe32Plus);
    }
    let mut offset = nt_off;
    let (nt, data_dirs) =
        pe::ImageNtHeaders64::parse(data, &mut offset).map_err(|_| Pe64Error::InvalidPe)?;
    let num_sections = usize::from(nt.file_header.number_of_sections.get(LE));
    let opt_size = usize::from(nt.file_header.size_of_optional_header.get(LE));
    if num_sections == 0 || num_sections > max_sections || opt_size > 1024 * 1024 {
        return Err(Pe64Error::InvalidPe);
    }
    let sections = nt
        .sections(data, offset)
        .map_err(|_| Pe64Error::InvalidPe)?;
    Ok(Pe64 {
        machine: nt.file_header.machine.get(LE).0,
        image_base: nt.optional_header.image_base.get(LE),
        data_dirs,
        sections,
    })
}

/// Translate an RVA to a file offset using `max(VirtualSize, SizeOfRawData)` per
/// PE section header.
pub(crate) fn pe_rva_to_file_offset(
    sections: &object::read::pe::SectionTable<'_>,
    rva: u32,
) -> Option<u64> {
    for sec in sections.iter() {
        let va = sec.virtual_address.get(LE);
        let span = sec.virtual_size.get(LE).max(sec.size_of_raw_data.get(LE));
        if rva >= va && rva - va < span {
            return u64::from(sec.pointer_to_raw_data.get(LE)).checked_add(u64::from(rva - va));
        }
    }
    None
}

/// Error returned when reading a PE32+ export directory.
pub(crate) enum PeExportError {
    MissingExportTable,
    Malformed,
}

/// Walk named, non-forwarded exports of a PE32+ image without reading entire
/// `.rdata` / `.text` sections into memory.
pub(crate) fn walk_pe64_exports<'data, R: ReadRef<'data>>(
    data: R,
    pe: &Pe64<'data>,
    max_symbols: u32,
    mut visit: impl FnMut(Option<&'data str>, u32, Option<u64>),
) -> Result<(), PeExportError> {
    let Some(dir_entry) = pe.data_dirs.get(pe::IMAGE_DIRECTORY_ENTRY_EXPORT) else {
        return Err(PeExportError::MissingExportTable);
    };
    let export_rva = dir_entry.virtual_address.get(LE);
    let export_size = dir_entry.size.get(LE);
    if export_rva == 0 || export_size < 40 {
        return Err(PeExportError::MissingExportTable);
    }
    let export_off =
        pe_rva_to_file_offset(&pe.sections, export_rva).ok_or(PeExportError::Malformed)?;
    let dir = data
        .read_at::<pe::ImageExportDirectory>(export_off)
        .map_err(|_| PeExportError::Malformed)?;
    let name_count = dir.number_of_names.get(LE);
    let func_count = dir.number_of_functions.get(LE);
    if name_count == 0 || name_count > max_symbols || func_count > max_symbols {
        return Err(PeExportError::Malformed);
    }
    let names_off = pe_rva_to_file_offset(&pe.sections, dir.address_of_names.get(LE))
        .ok_or(PeExportError::Malformed)?;
    let ordinals_off = pe_rva_to_file_offset(&pe.sections, dir.address_of_name_ordinals.get(LE))
        .ok_or(PeExportError::Malformed)?;
    let funcs_rva = dir.address_of_functions.get(LE);
    let names = data
        .read_slice_at::<U32<LittleEndian>>(names_off, name_count as usize)
        .map_err(|_| PeExportError::Malformed)?;
    let ordinals = data
        .read_slice_at::<U16<LittleEndian>>(ordinals_off, name_count as usize)
        .map_err(|_| PeExportError::Malformed)?;

    let export_end = export_rva.saturating_add(export_size);
    for (name_ptr, ord_val) in names.iter().zip(ordinals.iter()) {
        let ordinal = ord_val.get(LE);
        if func_count > 0 && u32::from(ordinal) >= func_count {
            continue;
        }
        let Some(func_rva_addr) = funcs_rva.checked_add(u32::from(ordinal).saturating_mul(4))
        else {
            continue;
        };
        let Some(func_off) = pe_rva_to_file_offset(&pe.sections, func_rva_addr) else {
            continue;
        };
        let Ok(rva_u32) = data.read_at::<U32<LittleEndian>>(func_off) else {
            continue;
        };
        let rva = rva_u32.get(LE);
        if rva == 0 || (rva >= export_rva && rva < export_end) {
            continue;
        }
        let name = pe_rva_to_file_offset(&pe.sections, name_ptr.get(LE))
            .and_then(|off| read_cstr_at(data, off, 256));
        let target_file_off = pe_rva_to_file_offset(&pe.sections, rva);
        visit(name, rva, target_file_off);
    }
    Ok(())
}

/// Read the CodeView `RSDS` `(guid, age, path)` debug record from a PE32 or
/// PE32+ image without scanning or loading entire sections.
pub(crate) fn read_pe_codeview<'data, R: ReadRef<'data>>(
    data: R,
) -> Option<([u8; 16], u32, String)> {
    let dos = pe::ImageDosHeader::parse(data).ok()?;
    let nt_off = u64::from(dos.nt_headers_offset());
    if nt_off > 1024 * 1024 {
        return None;
    }
    let magic = object::read::pe::optional_header_magic(data).ok()?;
    let mut offset = nt_off;
    let (data_dirs, sections) = match magic {
        pe::IMAGE_NT_OPTIONAL_HDR64_MAGIC => {
            let (nt, dirs) = pe::ImageNtHeaders64::parse(data, &mut offset).ok()?;
            let nsec = usize::from(nt.file_header.number_of_sections.get(LE));
            if nsec > 512 {
                return None;
            }
            (dirs, nt.sections(data, offset).ok()?)
        }
        pe::IMAGE_NT_OPTIONAL_HDR32_MAGIC => {
            let (nt, dirs) = pe::ImageNtHeaders32::parse(data, &mut offset).ok()?;
            let nsec = usize::from(nt.file_header.number_of_sections.get(LE));
            if nsec > 512 {
                return None;
            }
            (dirs, nt.sections(data, offset).ok()?)
        }
        _ => return None,
    };
    let debug_entry = data_dirs.get(pe::IMAGE_DIRECTORY_ENTRY_DEBUG)?;
    let debug_rva = debug_entry.virtual_address.get(LE);
    let debug_size = debug_entry.size.get(LE) as usize;
    let rec_size = std::mem::size_of::<pe::ImageDebugDirectory>();
    if debug_rva == 0 || !(rec_size..=1024 * 1024).contains(&debug_size) {
        return None;
    }
    let debug_off = pe_rva_to_file_offset(&sections, debug_rva)?;
    let debug_dirs = data
        .read_slice_at::<pe::ImageDebugDirectory>(debug_off, debug_size / rec_size)
        .ok()?;
    for rec in debug_dirs {
        if rec.typ.get(LE) != pe::IMAGE_DEBUG_TYPE_CODEVIEW {
            continue;
        }
        let size = rec.size_of_data.get(LE) as usize;
        let ptr = u64::from(rec.pointer_to_raw_data.get(LE));
        if !(24..1024 * 1024).contains(&size) || ptr == 0 {
            continue;
        }
        let raw = data.read_bytes_at(ptr, size as u64).ok()?;
        let mut cur = Bytes(raw);
        if cur.read_bytes(4).ok()?.0 != b"RSDS" {
            continue;
        }
        let guid: [u8; 16] = cur.read_bytes(16).ok()?.0.try_into().ok()?;
        let age = cur.read::<U32<LittleEndian>>().ok()?.get(LE);
        let rest = cur.0;
        let end = rest.iter().position(|&b| b == 0).unwrap_or(rest.len());
        let path = std::str::from_utf8(&rest[..end]).ok()?.trim();
        if !path.is_empty() {
            return Some((guid, age, path.to_string()));
        }
    }
    None
}

fn read_cstr_at<'data, R: ReadRef<'data>>(
    data: R,
    offset: u64,
    max_len: u64,
) -> Option<&'data str> {
    let total = data.len().ok()?;
    let avail = total.checked_sub(offset)?;
    if avail == 0 {
        return None;
    }
    let slice = data.read_bytes_at(offset, avail.min(max_len)).ok()?;
    let end = slice.iter().position(|&b| b == 0).unwrap_or(slice.len());
    std::str::from_utf8(&slice[..end]).ok()
}
