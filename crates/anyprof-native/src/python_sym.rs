//! Locates `_PyRuntime` in a CPython image without attaching to the process.
//!
//! `Py_Version` and `_PyEval_EvalFrameDefault` are recorded when they are
//! exported. Detection does not require them: 3.10 exports `_PyRuntime` and
//! does not export `Py_Version` or `_PyEval_EvalFrameDefault`. The frame walk
//! then refuses that process instead of guessing a layout. ELF dynamic
//! symbols, Mach-O nlist entries, and PE exports are enough. A stripped
//! interpreter that no longer exports `_PyRuntime` is not treated as CPython.
//! PyPy does not export that symbol.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use crate::elf::LoadSegment;

const MAX_SYMBOLS: u32 = 2_000_000;
const MAX_STRINGS: u32 = 64 * 1024 * 1024;
const MAX_SECTIONS: u16 = 10_000;
const MAX_COMMANDS: u32 = 10_000;
const MAX_COMMAND_BYTES: u32 = 16 * 1024 * 1024;

/// A gap larger than this is not the body of one function.
const MAX_FUNCTION_SPAN: u64 = 1024 * 1024;

const RUNTIME: &str = "_PyRuntime";
const VERSION: &str = "Py_Version";
const EVAL: &str = "_PyEval_EvalFrameDefault";

/// Link addresses of the three symbols, plus enough to relocate them.
#[derive(Debug)]
#[allow(dead_code)]
pub(crate) struct PythonImage {
    pub runtime: u64,
    pub runtime_file: u64,
    pub version: u64,
    /// File offset of `Py_Version`. `None` when that symbol is not exported.
    pub version_file: Option<u64>,
    pub eval: u64,
    /// Zero when the format does not record a trustworthy size.
    pub eval_size: u64,
    pub loads: Vec<LoadSegment>,
    /// Mach-O `__TEXT` vmaddr. Zero for ELF and PE.
    pub text_vmaddr: u64,
    /// PE preferred image base. Zero for ELF and Mach-O.
    pub image_base: u64,
}

/// Reads the three CPython symbols from `path`.
pub(crate) fn exports(path: &Path) -> Option<PythonImage> {
    let mut file = File::open(path).ok()?;
    let mut magic = [0u8; 4];
    file.read_exact(&mut magic).ok()?;
    file.seek(SeekFrom::Start(0)).ok()?;
    let le = u32::from_le_bytes(magic);
    let be = u32::from_be_bytes(magic);
    if le == 0x464c_457f {
        return elf_exports(&mut file);
    }
    if le == 0xFEED_FACF || be == 0xCAFE_BABE {
        return macho_exports(&mut file);
    }
    // A DOS header is `MZ` (`4D 5A`), usually followed by `0x90`. The
    // little-endian word is `0x00905A4D`, not `0x00005A4D`.
    if magic[0] == b'M' && magic[1] == b'Z' {
        return pe_exports(&mut file);
    }
    None
}

/// Mach-O load commands that name a Python runtime library.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn linked_python_libraries(path: &Path) -> Vec<std::path::PathBuf> {
    let Ok(mut file) = File::open(path) else {
        return Vec::new();
    };
    let Some((offset, _)) = macho_slice(&mut file) else {
        return Vec::new();
    };
    let Some((commands, ncmds)) = macho_commands(&mut file, offset) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut cursor = 0usize;
    for _ in 0..ncmds {
        if commands.len() < cursor + 8 {
            break;
        }
        let cmd = u32::from_le_bytes(commands[cursor..cursor + 4].try_into().unwrap_or([0; 4]));
        let size = u32::from_le_bytes(
            commands[cursor + 4..cursor + 8]
                .try_into()
                .unwrap_or([0; 4]),
        ) as usize;
        if size < 8 || cursor + size > commands.len() {
            break;
        }
        // LC_LOAD_DYLIB, LC_LOAD_WEAK_DYLIB, LC_REEXPORT_DYLIB.
        if matches!(cmd, 0xC | 0x18 | 0x1F) && size >= 24 {
            let name_off = u32::from_le_bytes(
                commands[cursor + 8..cursor + 12]
                    .try_into()
                    .unwrap_or([0; 4]),
            ) as usize;
            if name_off >= 8 && name_off < size {
                let bytes = &commands[cursor + name_off..cursor + size];
                let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
                let name = String::from_utf8_lossy(&bytes[..end]);
                if name_mentions_python(&name)
                    && let Some(resolved) = resolve_install_name(path, &name)
                {
                    out.push(resolved);
                }
            }
        }
        cursor += size;
    }
    out
}

fn name_mentions_python(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.contains("python") || lower.contains("libpython")
}

fn resolve_install_name(binary: &Path, name: &str) -> Option<std::path::PathBuf> {
    let dir = binary.parent()?;
    let candidate = if let Some(rest) = name.strip_prefix("@executable_path/") {
        dir.join(rest)
    } else if let Some(rest) = name.strip_prefix("@loader_path/") {
        dir.join(rest)
    } else if let Some(rest) = name.strip_prefix("@rpath/") {
        let direct = dir.join(rest);
        if direct.is_file() {
            direct
        } else {
            dir.join("../lib").join(rest)
        }
    } else {
        std::path::PathBuf::from(name)
    };
    candidate.is_file().then_some(candidate)
}

fn read_at(file: &mut File, offset: u64, buf: &mut [u8]) -> Option<()> {
    file.seek(SeekFrom::Start(offset)).ok()?;
    file.read_exact(buf).ok()
}

fn read_vec(file: &mut File, offset: u64, len: usize) -> Option<Vec<u8>> {
    if len > MAX_STRINGS as usize {
        return None;
    }
    let mut buf = vec![0u8; len];
    read_at(file, offset, &mut buf)?;
    Some(buf)
}

fn elf_exports(file: &mut File) -> Option<PythonImage> {
    let mut ident = [0u8; 16];
    read_at(file, 0, &mut ident)?;
    if ident[4] != 2 || ident[5] != 1 {
        return None;
    }
    let mut header = [0u8; 64];
    read_at(file, 0, &mut header)?;
    let phoff = u64::from_le_bytes(header[32..40].try_into().ok()?);
    let shoff = u64::from_le_bytes(header[40..48].try_into().ok()?);
    let phentsize = u16::from_le_bytes(header[54..56].try_into().ok()?);
    let phnum = u16::from_le_bytes(header[56..58].try_into().ok()?);
    let shentsize = u16::from_le_bytes(header[58..60].try_into().ok()?);
    let mut shnum = u16::from_le_bytes(header[60..62].try_into().ok()?);
    let mut shstrndx = u16::from_le_bytes(header[62..64].try_into().ok()?);
    if phentsize < 56 || phnum == 0 || phnum > 512 || shentsize < 64 || shoff == 0 {
        return None;
    }
    let loads = elf_loads(file, phoff, phentsize, phnum)?;
    if shnum == 0 {
        let mut zero = [0u8; 64];
        read_at(file, shoff, &mut zero)?;
        shnum = u32::from_le_bytes(zero[44..48].try_into().ok()?)
            .try_into()
            .ok()?;
        if shstrndx == 0xffff {
            shstrndx = u32::from_le_bytes(zero[40..44].try_into().ok()?)
                .try_into()
                .ok()?;
        }
    }
    if shnum == 0 || shnum > MAX_SECTIONS || shstrndx >= shnum {
        return None;
    }
    let sections = read_vec(file, shoff, usize::from(shnum) * usize::from(shentsize))?;
    let mut runtime = None;
    let mut version = None;
    let mut eval = None;
    let mut eval_size = 0u64;
    for index in 0..shnum {
        let start = usize::from(index) * usize::from(shentsize);
        let row = sections.get(start..start + 64)?;
        let kind = u32::from_le_bytes(row[4..8].try_into().ok()?);
        // SHT_DYNSYM and SHT_SYMTAB. Ubuntu's interpreter keeps only the first.
        if kind != 11 && kind != 2 {
            continue;
        }
        let offset = u64::from_le_bytes(row[24..32].try_into().ok()?);
        let size = u64::from_le_bytes(row[32..40].try_into().ok()?);
        let link = u32::from_le_bytes(row[40..44].try_into().ok()?);
        let entsize = u64::from_le_bytes(row[56..64].try_into().ok()?);
        if entsize < 24 || link >= u32::from(shnum) {
            continue;
        }
        let count = size / entsize;
        if count == 0 || count > u64::from(MAX_SYMBOLS) {
            continue;
        }
        let str_row = sections
            .get(usize::try_from(link).ok()? * usize::from(shentsize)..)?
            .get(..64)?;
        let str_off = u64::from_le_bytes(str_row[24..32].try_into().ok()?);
        let str_size = u64::from_le_bytes(str_row[32..40].try_into().ok()?);
        if str_size == 0 || str_size > u64::from(MAX_STRINGS) {
            continue;
        }
        let strings = read_vec(file, str_off, usize::try_from(str_size).ok()?)?;
        let symbols = read_vec(file, offset, usize::try_from(count * entsize).ok()?)?;
        for sym in 0..count {
            let at = usize::try_from(sym * entsize).ok()?;
            let entry = symbols.get(at..at + 24)?;
            let name_off = u32::from_le_bytes(entry[0..4].try_into().ok()?) as usize;
            let info = entry[4];
            let shndx = u16::from_le_bytes(entry[6..8].try_into().ok()?);
            let value = u64::from_le_bytes(entry[8..16].try_into().ok()?);
            let sym_size = u64::from_le_bytes(entry[16..24].try_into().ok()?);
            let kind = info & 0xf;
            if shndx == 0 || value == 0 || !matches!(kind, 1 | 2 | 10) {
                continue;
            }
            let Some(name) = c_string(&strings, name_off) else {
                continue;
            };
            if name == RUNTIME {
                let Some(file_off) = file_offset(&loads, value) else {
                    continue;
                };
                runtime = Some((value, file_off));
            } else if name == VERSION {
                let Some(file_off) = file_offset(&loads, value) else {
                    continue;
                };
                version = Some((value, file_off));
            } else if name == EVAL {
                eval = Some(value);
                eval_size = sym_size;
            }
        }
    }
    let (runtime, runtime_file) = runtime?;
    let (version, version_file) = match version {
        Some((version, file)) => (version, Some(file)),
        None => (0, None),
    };
    let eval = eval.unwrap_or(0);
    Some(PythonImage {
        runtime,
        runtime_file,
        version,
        version_file,
        eval,
        eval_size,
        loads,
        text_vmaddr: 0,
        image_base: 0,
    })
}

fn elf_loads(file: &mut File, phoff: u64, entsize: u16, phnum: u16) -> Option<Vec<LoadSegment>> {
    let bytes = read_vec(file, phoff, usize::from(phnum) * usize::from(entsize))?;
    let mut loads = Vec::new();
    for index in 0..phnum {
        let start = usize::from(index) * usize::from(entsize);
        let row = bytes.get(start..start + 56)?;
        let kind = u32::from_le_bytes(row[0..4].try_into().ok()?);
        if kind != 1 {
            continue;
        }
        loads.push(LoadSegment {
            offset: u64::from_le_bytes(row[8..16].try_into().ok()?),
            vaddr: u64::from_le_bytes(row[16..24].try_into().ok()?),
            filesz: u64::from_le_bytes(row[32..40].try_into().ok()?),
        });
    }
    (!loads.is_empty()).then_some(loads)
}

fn file_offset(loads: &[LoadSegment], vaddr: u64) -> Option<u64> {
    loads.iter().find_map(|load| {
        if vaddr >= load.vaddr && vaddr < load.vaddr.saturating_add(load.filesz) {
            Some(load.offset + (vaddr - load.vaddr))
        } else {
            None
        }
    })
}

fn c_string(bytes: &[u8], offset: usize) -> Option<&str> {
    let rest = bytes.get(offset..)?;
    let end = rest.iter().position(|b| *b == 0)?;
    std::str::from_utf8(&rest[..end]).ok()
}

fn macho_slice(file: &mut File) -> Option<(u64, u64)> {
    let len = file.seek(SeekFrom::End(0)).ok()?;
    let mut magic = [0u8; 4];
    read_at(file, 0, &mut magic)?;
    if u32::from_le_bytes(magic) == 0xFEED_FACF {
        return Some((0, len));
    }
    if u32::from_be_bytes(magic) != 0xCAFE_BABE {
        return None;
    }
    let mut count = [0u8; 4];
    read_at(file, 4, &mut count)?;
    let nfat = u32::from_be_bytes(count);
    if nfat == 0 || nfat > 64 {
        return None;
    }
    let want = host_cputype();
    for index in 0..nfat {
        let mut arch = [0u8; 20];
        read_at(file, 8 + u64::from(index) * 20, &mut arch)?;
        let cputype = u32::from_be_bytes(arch[0..4].try_into().ok()?);
        let offset = u64::from(u32::from_be_bytes(arch[8..12].try_into().ok()?));
        let size = u64::from(u32::from_be_bytes(arch[12..16].try_into().ok()?));
        if cputype == want {
            return Some((offset, size));
        }
    }
    None
}

fn host_cputype() -> u32 {
    #[cfg(target_arch = "aarch64")]
    {
        0x0100_000C
    }
    #[cfg(not(target_arch = "aarch64"))]
    {
        0x0100_0007
    }
}

fn macho_commands(file: &mut File, slice: u64) -> Option<(Vec<u8>, usize)> {
    let mut header = [0u8; 32];
    read_at(file, slice, &mut header)?;
    if u32::from_le_bytes(header[0..4].try_into().ok()?) != 0xFEED_FACF {
        return None;
    }
    let ncmds = u32::from_le_bytes(header[16..20].try_into().ok()?);
    let sizeofcmds = u32::from_le_bytes(header[20..24].try_into().ok()?);
    if ncmds == 0 || ncmds > MAX_COMMANDS || sizeofcmds == 0 || sizeofcmds > MAX_COMMAND_BYTES {
        return None;
    }
    let commands = read_vec(file, slice + 32, sizeofcmds as usize)?;
    Some((commands, ncmds as usize))
}

fn macho_exports(file: &mut File) -> Option<PythonImage> {
    let (slice, _) = macho_slice(file)?;
    let (commands, ncmds) = macho_commands(file, slice)?;
    let mut text_vmaddr = None;
    let mut symoff = 0u64;
    let mut nsyms = 0u32;
    let mut stroff = 0u64;
    let mut strsize = 0u32;
    let mut cursor = 0usize;
    let mut segments = Vec::new();
    for _ in 0..ncmds {
        if commands.len() < cursor + 8 {
            return None;
        }
        let cmd = u32::from_le_bytes(commands[cursor..cursor + 4].try_into().ok()?);
        let size = u32::from_le_bytes(commands[cursor + 4..cursor + 8].try_into().ok()?) as usize;
        if size < 8 || cursor + size > commands.len() {
            return None;
        }
        let data = &commands[cursor + 8..cursor + size];
        if cmd == 0x19 && data.len() >= 64 {
            let name = std::str::from_utf8(&data[0..16])
                .ok()?
                .trim_end_matches('\0');
            let vmaddr = u64::from_le_bytes(data[16..24].try_into().ok()?);
            let fileoff = u64::from_le_bytes(data[32..40].try_into().ok()?);
            let filesize = u64::from_le_bytes(data[40..48].try_into().ok()?);
            if name == "__TEXT" {
                text_vmaddr = Some(vmaddr);
            }
            segments.push((vmaddr, fileoff, filesize));
        } else if cmd == 0x2 && data.len() >= 16 {
            symoff = u64::from(u32::from_le_bytes(data[0..4].try_into().ok()?));
            nsyms = u32::from_le_bytes(data[4..8].try_into().ok()?);
            stroff = u64::from(u32::from_le_bytes(data[8..12].try_into().ok()?));
            strsize = u32::from_le_bytes(data[12..16].try_into().ok()?);
        }
        cursor += size;
    }
    if nsyms == 0 || nsyms > MAX_SYMBOLS || strsize == 0 || strsize > MAX_STRINGS {
        return None;
    }
    let strings = read_vec(file, slice + stroff, strsize as usize)?;
    let symbols = read_vec(file, slice + symoff, nsyms as usize * 16)?;
    let mut values = Vec::new();
    let mut runtime = None;
    let mut version = None;
    let mut eval = None;
    for index in 0..nsyms as usize {
        let entry = symbols.get(index * 16..index * 16 + 16)?;
        let strx = u32::from_le_bytes(entry[0..4].try_into().ok()?) as usize;
        let n_type = entry[4];
        let n_sect = entry[5];
        let value = u64::from_le_bytes(entry[8..16].try_into().ok()?);
        if n_type & 0xe0 != 0 || n_type & 0x0e != 0x0e || n_sect == 0 || value == 0 {
            continue;
        }
        values.push(value);
        let Some(name) = c_string(&strings, strx) else {
            continue;
        };
        let name = name.strip_prefix('_').unwrap_or(name);
        if name == RUNTIME {
            let Some(file_off) = macho_file_offset(slice, &segments, value) else {
                continue;
            };
            runtime = Some((value, file_off));
        } else if name == VERSION {
            let Some(file_off) = macho_file_offset(slice, &segments, value) else {
                continue;
            };
            version = Some((value, file_off));
        } else if name == EVAL {
            eval = Some(value);
        }
    }
    values.sort_unstable();
    values.dedup();
    let eval_size = eval.map(|value| next_gap(&values, value)).unwrap_or(0);
    let eval = eval.unwrap_or(0);
    let (runtime, runtime_file) = runtime?;
    let (version, version_file) = match version {
        Some((version, file)) => (version, Some(file)),
        None => (0, None),
    };
    Some(PythonImage {
        runtime,
        runtime_file,
        version,
        version_file,
        eval,
        eval_size,
        loads: Vec::new(),
        text_vmaddr: text_vmaddr?,
        image_base: 0,
    })
}

fn macho_file_offset(slice: u64, segments: &[(u64, u64, u64)], value: u64) -> Option<u64> {
    segments.iter().find_map(|(vmaddr, fileoff, filesize)| {
        if value >= *vmaddr && value < vmaddr.saturating_add(*filesize) {
            Some(slice + fileoff + (value - vmaddr))
        } else {
            None
        }
    })
}

fn next_gap(sorted: &[u64], value: u64) -> u64 {
    let Some(pos) = sorted.iter().position(|item| *item == value) else {
        return 0;
    };
    let Some(next) = sorted.get(pos + 1).copied() else {
        return 0;
    };
    let gap = next.saturating_sub(value);
    if (1..=MAX_FUNCTION_SPAN).contains(&gap) {
        gap
    } else {
        0
    }
}

fn pe_exports(file: &mut File) -> Option<PythonImage> {
    let mut dos = [0u8; 0x40];
    read_at(file, 0, &mut dos)?;
    let e_lfanew = u32::from_le_bytes(dos[0x3c..0x40].try_into().ok()?) as u64;
    let mut sig = [0u8; 4];
    read_at(file, e_lfanew, &mut sig)?;
    if sig != [b'P', b'E', 0, 0] {
        return None;
    }
    let mut coff = [0u8; 20];
    read_at(file, e_lfanew + 4, &mut coff)?;
    let nsections = u16::from_le_bytes(coff[2..4].try_into().ok()?);
    let optsize = u16::from_le_bytes(coff[16..18].try_into().ok()?);
    if nsections == 0 || nsections > 128 || optsize < 112 {
        return None;
    }
    let optional = read_vec(file, e_lfanew + 24, usize::from(optsize))?;
    if u16::from_le_bytes(optional[0..2].try_into().ok()?) != 0x20b {
        return None;
    }
    let image_base = u64::from_le_bytes(optional.get(24..32)?.try_into().ok()?);
    let export_rva = u32::from_le_bytes(optional.get(112..116)?.try_into().ok()?) as u64;
    let export_size = u32::from_le_bytes(optional.get(116..120)?.try_into().ok()?) as u64;
    if export_rva == 0 || export_size < 40 {
        return None;
    }
    let sections = read_vec(
        file,
        e_lfanew + 24 + u64::from(optsize),
        usize::from(nsections) * 40,
    )?;
    let mut mapped = Vec::new();
    for index in 0..usize::from(nsections) {
        let row = sections.get(index * 40..index * 40 + 40)?;
        mapped.push(PeSection {
            va: u32::from_le_bytes(row[12..16].try_into().ok()?) as u64,
            virtual_size: u32::from_le_bytes(row[8..12].try_into().ok()?) as u64,
            raw: u32::from_le_bytes(row[20..24].try_into().ok()?) as u64,
            raw_size: u32::from_le_bytes(row[16..20].try_into().ok()?) as u64,
        });
    }
    let export_off = pe_file_offset(&mapped, export_rva)?;
    let mut dir = [0u8; 40];
    read_at(file, export_off, &mut dir)?;
    let nnames = u32::from_le_bytes(dir[24..28].try_into().ok()?);
    let functions = u32::from_le_bytes(dir[28..32].try_into().ok()?) as u64;
    let names = u32::from_le_bytes(dir[32..36].try_into().ok()?) as u64;
    let ordinals = u32::from_le_bytes(dir[36..40].try_into().ok()?) as u64;
    if nnames == 0 || nnames > MAX_SYMBOLS {
        return None;
    }
    let names_off = pe_file_offset(&mapped, names)?;
    let name_table = read_vec(file, names_off, nnames as usize * 4)?;
    let ordinal_table = read_vec(
        file,
        pe_file_offset(&mapped, ordinals)?,
        nnames as usize * 2,
    )?;
    let mut runtime = None;
    let mut version = None;
    let mut eval = None;
    let mut rvas = Vec::new();
    for index in 0..nnames as usize {
        let Some(name_bytes) = name_table.get(index * 4..index * 4 + 4) else {
            continue;
        };
        let Some(ordinal_bytes) = ordinal_table.get(index * 2..index * 2 + 2) else {
            continue;
        };
        let name_rva = u32::from_le_bytes(name_bytes.try_into().unwrap()) as u64;
        let ordinal = u16::from_le_bytes(ordinal_bytes.try_into().unwrap());
        let Some(func_off) = pe_file_offset(&mapped, functions + u64::from(ordinal) * 4) else {
            continue;
        };
        let mut rva_buf = [0u8; 4];
        if read_at(file, func_off, &mut rva_buf).is_none() {
            continue;
        }
        let rva = u32::from_le_bytes(rva_buf) as u64;
        if rva == 0 || (rva >= export_rva && rva < export_rva + export_size) {
            continue;
        }
        rvas.push(rva);
        let Some(name_off) = pe_file_offset(&mapped, name_rva) else {
            continue;
        };
        let mut name_buf = [0u8; 64];
        if read_at(file, name_off, &mut name_buf).is_none() {
            continue;
        }
        let end = name_buf
            .iter()
            .position(|b| *b == 0)
            .unwrap_or(name_buf.len());
        let Ok(name) = std::str::from_utf8(&name_buf[..end]) else {
            continue;
        };
        let link = image_base.saturating_add(rva);
        let Some(file_off) = pe_file_offset(&mapped, rva) else {
            continue;
        };
        if name == RUNTIME {
            runtime = Some((link, file_off));
        } else if name == VERSION {
            version = Some((link, file_off));
        } else if name == EVAL {
            eval = Some(link);
        }
    }
    rvas.sort_unstable();
    rvas.dedup();
    let (runtime, runtime_file) = runtime?;
    let (version, version_file) = match version {
        Some((version, file)) => (version, Some(file)),
        None => (0, None),
    };
    let eval_size = eval
        .map(|value| next_gap(&rvas, value.saturating_sub(image_base)))
        .unwrap_or(0);
    let eval = eval.unwrap_or(0);
    Some(PythonImage {
        runtime,
        runtime_file,
        version,
        version_file,
        eval,
        eval_size,
        loads: Vec::new(),
        text_vmaddr: 0,
        image_base,
    })
}

struct PeSection {
    va: u64,
    virtual_size: u64,
    raw: u64,
    raw_size: u64,
}

fn pe_file_offset(sections: &[PeSection], rva: u64) -> Option<u64> {
    sections.iter().find_map(|section| {
        let span = section.virtual_size.max(section.raw_size);
        if rva >= section.va && rva < section.va.saturating_add(span) {
            Some(section.raw + (rva - section.va))
        } else {
            None
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_embed_dll_exports_the_runtime() {
        let path = Path::new("/tmp/py313embed/python313.dll");
        if !path.is_file() {
            return;
        }
        let image = exports(path).expect("python313.dll exports");
        assert!(image.runtime > 0);
        assert!(image.version > 0);
        assert!(image.image_base > 0);
        let mut file = File::open(path).unwrap();
        let mut version = [0u8; 4];
        read_at(
            &mut file,
            image.version_file.expect("Py_Version"),
            &mut version,
        )
        .unwrap();
        let hex = u32::from_le_bytes(version);
        assert_eq!(hex >> 24, 3, "{hex:#x}");
        assert_eq!((hex >> 16) & 0xff, 13, "{hex:#x}");
        let mut cookie = [0u8; 8];
        read_at(&mut file, image.runtime_file, &mut cookie).unwrap();
        assert_eq!(&cookie, b"xdebugpy");
    }

    #[test]
    fn local_cpython_exports_the_runtime() {
        let path = Path::new("/usr/bin/python3.12");
        if !path.is_file() {
            return;
        }
        let image = exports(path).expect("python3.12 exports");
        assert!(image.runtime > 0);
        assert!(image.version > 0);
        assert!(image.eval > 0);
        assert!(image.eval_size > 1024, "eval size {}", image.eval_size);
        let mut file = File::open(path).unwrap();
        let mut version = [0u8; 4];
        read_at(
            &mut file,
            image.version_file.expect("Py_Version"),
            &mut version,
        )
        .unwrap();
        let hex = u32::from_le_bytes(version);
        assert_eq!(hex >> 24, 3, "{hex:#x}");
        assert_eq!((hex >> 16) & 0xff, 12, "{hex:#x}");
    }

    #[test]
    fn a_real_mz_header_is_a_pe_image() {
        let bytes = synthetic_pe(true);
        let path = std::env::temp_dir().join(format!("anyprof-mz-{}.dll", std::process::id()));
        std::fs::write(&path, &bytes).unwrap();
        let image = exports(&path).expect("MZ image exports _PyRuntime");
        assert_eq!(image.image_base, 0x1_8000_0000);
        assert!(image.version_file.is_some());
        assert!(image.eval > 0);
        let mut file = File::open(&path).unwrap();
        let mut version = [0u8; 4];
        read_at(&mut file, image.version_file.unwrap(), &mut version).unwrap();
        assert_eq!(u32::from_le_bytes(version), 0x030D_12F0);
        let mut cookie = [0u8; 8];
        read_at(&mut file, image.runtime_file, &mut cookie).unwrap();
        assert_eq!(&cookie, b"xdebugpy");

        std::fs::write(&path, synthetic_pe(false)).unwrap();
        let image = exports(&path).expect("runtime alone is still CPython");
        assert!(image.version_file.is_none());
        assert_eq!(image.eval, 0);
        let _ = std::fs::remove_file(&path);
    }

    /// A tiny PE32+ whose DOS header is the usual `MZ` plus `0x90`, not a
    /// zero-padded `MZ` word. `with_symbols` also exports `Py_Version` and
    /// `_PyEval_EvalFrameDefault`.
    fn synthetic_pe(with_symbols: bool) -> Vec<u8> {
        let mut bytes = vec![0u8; 0x400];
        bytes[0] = b'M';
        bytes[1] = b'Z';
        bytes[2] = 0x90;
        bytes[0x3c..0x40].copy_from_slice(&0x80u32.to_le_bytes());
        bytes[0x80..0x84].copy_from_slice(b"PE\0\0");
        bytes[0x86..0x88].copy_from_slice(&1u16.to_le_bytes());
        bytes[0x94..0x96].copy_from_slice(&0xF0u16.to_le_bytes());
        let optional = 0x98;
        bytes[optional..optional + 2].copy_from_slice(&0x20bu16.to_le_bytes());
        bytes[optional + 24..optional + 32].copy_from_slice(&0x1_8000_0000u64.to_le_bytes());
        bytes[optional + 112..optional + 116].copy_from_slice(&0x200u32.to_le_bytes());
        bytes[optional + 116..optional + 120].copy_from_slice(&40u32.to_le_bytes());
        let section = 0x188;
        bytes[section + 8..section + 12].copy_from_slice(&0x200u32.to_le_bytes());
        bytes[section + 12..section + 16].copy_from_slice(&0x200u32.to_le_bytes());
        bytes[section + 16..section + 20].copy_from_slice(&0x200u32.to_le_bytes());
        bytes[section + 20..section + 24].copy_from_slice(&0x200u32.to_le_bytes());

        let names = if with_symbols { 3u32 } else { 1 };
        bytes[0x200 + 24..0x200 + 28].copy_from_slice(&names.to_le_bytes());
        bytes[0x200 + 28..0x200 + 32].copy_from_slice(&0x240u32.to_le_bytes());
        bytes[0x200 + 32..0x200 + 36].copy_from_slice(&0x260u32.to_le_bytes());
        bytes[0x200 + 36..0x200 + 40].copy_from_slice(&0x280u32.to_le_bytes());
        let rvas = if with_symbols {
            [0x310u32, 0x300, 0x320]
        } else {
            [0x310, 0, 0]
        };
        for (index, rva) in rvas.iter().enumerate() {
            let at = 0x240 + index * 4;
            bytes[at..at + 4].copy_from_slice(&rva.to_le_bytes());
            let ordinal_at = 0x280 + index * 2;
            bytes[ordinal_at..ordinal_at + 2].copy_from_slice(&(index as u16).to_le_bytes());
        }
        let labels: [&[u8]; 3] = [b"_PyRuntime", b"Py_Version", b"_PyEval_EvalFrameDefault"];
        let mut cursor = 0x2A0usize;
        for (index, label) in labels.iter().enumerate().take(names as usize) {
            let name_at = 0x260 + index * 4;
            bytes[name_at..name_at + 4].copy_from_slice(&(cursor as u32).to_le_bytes());
            bytes[cursor..cursor + label.len()].copy_from_slice(label);
            cursor += label.len() + 1;
        }
        bytes[0x300..0x304].copy_from_slice(&0x030D_12F0u32.to_le_bytes());
        bytes[0x310..0x318].copy_from_slice(b"xdebugpy");
        bytes
    }
}
