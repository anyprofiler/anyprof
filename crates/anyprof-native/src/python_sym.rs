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
const MAX_SECTIONS: u64 = 100_000;
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
    let len = file.metadata().ok()?.len();
    let mut magic = [0u8; 4];
    file.read_exact(&mut magic).ok()?;
    file.seek(SeekFrom::Start(0)).ok()?;
    let le = u32::from_le_bytes(magic);
    let be = u32::from_be_bytes(magic);
    let cache = object::ReadCache::new(file);
    if le == 0x464c_457f {
        return elf_exports(&cache);
    }
    if le == 0xFEED_FACF || be == 0xCAFE_BABE {
        return macho_exports(&cache, len);
    }
    // A DOS header is `MZ` (`4D 5A`), usually followed by `0x90`. The
    // little-endian word is `0x00905A4D`, not `0x00005A4D`.
    if magic[0] == b'M' && magic[1] == b'Z' {
        return pe_exports(&cache);
    }
    None
}

/// Mach-O load commands that name a Python runtime library.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn linked_python_libraries(path: &Path) -> Vec<std::path::PathBuf> {
    use object::LittleEndian;
    use object::macho;
    use object::read::macho::LoadCommandVariant;

    let Ok(file) = File::open(path) else {
        return Vec::new();
    };
    let Ok(meta) = file.metadata() else {
        return Vec::new();
    };
    let cache = object::ReadCache::new(file);
    let Some((slice_off, slice_size)) =
        crate::binfmt::macho_slice(&cache, meta.len(), crate::binfmt::macho_host_cputype())
    else {
        return Vec::new();
    };
    let slice_ref = cache.range(slice_off, slice_size);
    let Some((_, mut commands)) = crate::binfmt::macho_commands(slice_ref, MAX_COMMAND_BYTES)
    else {
        return Vec::new();
    };
    let mut out = Vec::new();
    while let Ok(Some(cmd)) = commands.next() {
        if matches!(
            cmd.cmd(),
            macho::LC_LOAD_DYLIB | macho::LC_LOAD_WEAK_DYLIB | macho::LC_REEXPORT_DYLIB
        ) && let Ok(LoadCommandVariant::Dylib(dylib)) = cmd.variant()
            && let Ok(raw) = cmd.string(LittleEndian, dylib.dylib.name)
        {
            let name = String::from_utf8_lossy(raw);
            if name_mentions_python(&name)
                && let Some(resolved) = resolve_install_name(path, &name)
            {
                out.push(resolved);
            }
        }
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

fn elf_exports(cache: &object::ReadCache<File>) -> Option<PythonImage> {
    use object::LittleEndian;
    use object::elf;
    use object::read::elf::Sym as _;

    let parsed = crate::binfmt::parse_elf64(cache, MAX_SECTIONS as usize)?;
    if parsed.loads.is_empty() || parsed.sections.is_empty() {
        return None;
    }
    let loads = parsed.loads;
    let mut runtime = None;
    let mut version = None;
    let mut eval = None;
    let mut eval_size = 0u64;
    for section_type in [elf::SHT_DYNSYM, elf::SHT_SYMTAB] {
        crate::binfmt::visit_elf64_symbols(
            cache,
            &parsed.sections,
            section_type,
            MAX_SYMBOLS as usize,
            u64::from(MAX_STRINGS),
            |name, sym| {
                let shndx = sym.st_shndx(LittleEndian);
                let value = sym.st_value(LittleEndian);
                let sym_size = sym.st_size(LittleEndian);
                let kind = sym.st_type();
                if shndx == elf::SHN_UNDEF
                    || value == 0
                    || !matches!(kind, elf::STT_OBJECT | elf::STT_FUNC | elf::STT_GNU_IFUNC)
                {
                    return;
                }
                if name == RUNTIME {
                    if let Some(file_off) = file_offset(&loads, value) {
                        runtime = Some((value, file_off));
                    }
                } else if name == VERSION {
                    if let Some(file_off) = file_offset(&loads, value) {
                        version = Some((value, file_off));
                    }
                } else if name == EVAL {
                    eval = Some(value);
                    eval_size = sym_size;
                }
            },
        );
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

fn file_offset(loads: &[LoadSegment], vaddr: u64) -> Option<u64> {
    loads.iter().find_map(|load| {
        if vaddr >= load.vaddr && vaddr < load.vaddr.saturating_add(load.filesz) {
            Some(load.offset + (vaddr - load.vaddr))
        } else {
            None
        }
    })
}

fn macho_exports(cache: &object::ReadCache<File>, len: u64) -> Option<PythonImage> {
    use object::LittleEndian;
    use object::macho;
    use object::read::macho::{LoadCommandVariant, Nlist as _};

    let (slice_off, slice_size) =
        crate::binfmt::macho_slice(cache, len, crate::binfmt::macho_host_cputype())?;
    let slice_ref = cache.range(slice_off, slice_size);
    let (_, mut commands) = crate::binfmt::macho_commands(slice_ref, MAX_COMMAND_BYTES)?;
    let mut text_vmaddr = None;
    let mut symtab_cmd = None;
    let mut segments = Vec::new();
    let mut seen_cmds = 0u32;
    while let Some(cmd) = commands.next().ok()? {
        seen_cmds += 1;
        if seen_cmds > MAX_COMMANDS {
            return None;
        }
        match cmd.variant().ok()? {
            LoadCommandVariant::Segment64(seg, _) => {
                let name = crate::binfmt::macho_name(&seg.segname);
                let vmaddr = seg.vmaddr.get(LittleEndian);
                let fileoff = seg.fileoff.get(LittleEndian);
                let filesize = seg.filesize.get(LittleEndian);
                if name == "__TEXT" {
                    text_vmaddr = Some(vmaddr);
                }
                segments.push((vmaddr, fileoff, filesize));
            }
            LoadCommandVariant::Symtab(symtab) => {
                symtab_cmd = Some(symtab);
            }
            _ => {}
        }
    }
    let symtab_cmd = symtab_cmd?;
    let nsyms = symtab_cmd.nsyms.get(LittleEndian);
    let strsize = symtab_cmd.strsize.get(LittleEndian);
    if nsyms == 0 || nsyms > MAX_SYMBOLS || strsize == 0 || strsize > MAX_STRINGS {
        return None;
    }
    let symtab = symtab_cmd
        .symbols::<macho::MachHeader64<LittleEndian>, _>(LittleEndian, slice_ref)
        .ok()?;
    let strings = symtab.strings();
    let mut values = Vec::new();
    let mut runtime = None;
    let mut version = None;
    let mut eval = None;
    for entry in symtab.symbols() {
        let n_type = entry.n_type().0;
        let n_sect = entry.n_sect();
        let value = entry.n_value(LittleEndian);
        if n_type & 0xe0 != 0 || n_type & 0x0e != 0x0e || n_sect == 0 || value == 0 {
            continue;
        }
        values.push(value);
        let Ok(raw_name) = entry.name(LittleEndian, strings) else {
            continue;
        };
        let Ok(name) = std::str::from_utf8(raw_name) else {
            continue;
        };
        let name = name.strip_prefix('_').unwrap_or(name);
        if name == RUNTIME {
            if let Some(file_off) = macho_file_offset(slice_off, &segments, value) {
                runtime = Some((value, file_off));
            }
        } else if name == VERSION {
            if let Some(file_off) = macho_file_offset(slice_off, &segments, value) {
                version = Some((value, file_off));
            }
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

fn pe_exports(cache: &object::ReadCache<File>) -> Option<PythonImage> {
    let pe = crate::binfmt::parse_pe64(cache, 128).ok()?;
    let image_base = pe.image_base;
    let mut runtime = None;
    let mut version = None;
    let mut eval = None;
    let mut rvas = Vec::new();
    crate::binfmt::walk_pe64_exports(cache, &pe, MAX_SYMBOLS, |name, rva, target_file_off| {
        let rva_u64 = u64::from(rva);
        rvas.push(rva_u64);
        let Some(name) = name else {
            return;
        };
        let Some(file_off) = target_file_off else {
            return;
        };
        let link = image_base.saturating_add(rva_u64);
        if name == RUNTIME {
            runtime = Some((link, file_off));
        } else if name == VERSION {
            version = Some((link, file_off));
        } else if name == EVAL {
            eval = Some(link);
        }
    })
    .ok()?;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn read_at(file: &mut File, offset: u64, buf: &mut [u8]) -> Option<()> {
        file.seek(SeekFrom::Start(offset)).ok()?;
        file.read_exact(buf).ok()
    }

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
        bytes[optional + 108..optional + 112].copy_from_slice(&16u32.to_le_bytes());
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

    #[test]
    fn elf64_extended_shnum_reads_section_zero_sh_size_not_sh_info() {
        let bytes = synthetic_extended_elf64();
        let path =
            std::env::temp_dir().join(format!("anyprof-ext-shnum-{}.elf", std::process::id()));
        std::fs::write(&path, &bytes).unwrap();
        let image = exports(&path).expect("extended ELF64 exports _PyRuntime");
        assert_eq!(image.runtime, 0x400310);
        assert_eq!(image.version, 0x400300);
        assert_eq!(image.version_file, Some(0x300));
        assert_eq!(image.eval, 0x400320);
        assert_eq!(image.eval_size, 0x80);
        let _ = std::fs::remove_file(&path);
    }

    fn synthetic_extended_elf64() -> Vec<u8> {
        let mut bytes = vec![0u8; 0x600];
        // ELF64 little-endian header.
        bytes[0..4].copy_from_slice(b"\x7fELF");
        bytes[4] = 2; // ELFCLASS64
        bytes[5] = 1; // ELFDATA2LSB
        bytes[6] = 1;
        bytes[16..18].copy_from_slice(&3u16.to_le_bytes()); // ET_DYN
        bytes[18..20].copy_from_slice(&62u16.to_le_bytes()); // EM_X86_64
        bytes[20..24].copy_from_slice(&1u32.to_le_bytes());
        let phoff = 0x40u64;
        let shoff = 0x100u64;
        bytes[32..40].copy_from_slice(&phoff.to_le_bytes());
        bytes[40..48].copy_from_slice(&shoff.to_le_bytes());
        bytes[52..54].copy_from_slice(&64u16.to_le_bytes());
        bytes[54..56].copy_from_slice(&56u16.to_le_bytes()); // e_phentsize
        bytes[56..58].copy_from_slice(&1u16.to_le_bytes()); // e_phnum
        bytes[58..60].copy_from_slice(&64u16.to_le_bytes()); // e_shentsize
        bytes[60..62].copy_from_slice(&0u16.to_le_bytes()); // e_shnum == 0 (extended)
        bytes[62..64].copy_from_slice(&0xffffu16.to_le_bytes()); // e_shstrndx == SHN_XINDEX

        // Program header 0: PT_LOAD mapping file [0..0x600) at vaddr 0x400000.
        let ph = phoff as usize;
        bytes[ph..ph + 4].copy_from_slice(&1u32.to_le_bytes()); // PT_LOAD
        bytes[ph + 4..ph + 8].copy_from_slice(&5u32.to_le_bytes());
        bytes[ph + 8..ph + 16].copy_from_slice(&0u64.to_le_bytes());
        bytes[ph + 16..ph + 24].copy_from_slice(&0x400000u64.to_le_bytes());
        bytes[ph + 32..ph + 40].copy_from_slice(&0x600u64.to_le_bytes());
        bytes[ph + 40..ph + 48].copy_from_slice(&0x600u64.to_le_bytes());

        // String table at 0x240.
        let str_off = 0x240usize;
        let mut strtab = vec![0u8];
        let name_runtime = strtab.len() as u32;
        strtab.extend_from_slice(b"_PyRuntime\0");
        let name_version = strtab.len() as u32;
        strtab.extend_from_slice(b"Py_Version\0");
        let name_eval = strtab.len() as u32;
        strtab.extend_from_slice(b"_PyEval_EvalFrameDefault\0");
        bytes[str_off..str_off + strtab.len()].copy_from_slice(&strtab);

        // Dynamic symbol table at 0x380 (4 entries * 24 bytes = 96 bytes).
        let sym_off = 0x380usize;
        let write_sym = |buf: &mut [u8], idx: usize, name: u32, info: u8, value: u64, size: u64| {
            let at = sym_off + idx * 24;
            buf[at..at + 4].copy_from_slice(&name.to_le_bytes());
            buf[at + 4] = info;
            buf[at + 6..at + 8].copy_from_slice(&1u16.to_le_bytes()); // st_shndx != 0
            buf[at + 8..at + 16].copy_from_slice(&value.to_le_bytes());
            buf[at + 16..at + 24].copy_from_slice(&size.to_le_bytes());
        };
        write_sym(&mut bytes, 1, name_runtime, 0x11, 0x400310, 64); // STT_OBJECT
        write_sym(&mut bytes, 2, name_version, 0x11, 0x400300, 4); // STT_OBJECT
        write_sym(&mut bytes, 3, name_eval, 0x12, 0x400320, 0x80); // STT_FUNC
        bytes[0x300..0x304].copy_from_slice(&0x030c_10f0u32.to_le_bytes());
        bytes[0x310..0x318].copy_from_slice(b"xdebugpy");

        // Section 0 at 0x100: sh_size [32..40] = 3 sections, sh_link [40..44] = 2 (shstrndx),
        // and sh_info [44..48] = 0xdead_beef (PN_XNUM slot; must NOT be read as shnum).
        let s0 = shoff as usize;
        bytes[s0 + 32..s0 + 40].copy_from_slice(&3u64.to_le_bytes());
        bytes[s0 + 40..s0 + 44].copy_from_slice(&2u32.to_le_bytes());
        bytes[s0 + 44..s0 + 48].copy_from_slice(&0xdead_beefu32.to_le_bytes());

        // Section 1 at 0x140: SHT_DYNSYM (11), link = 2.
        let s1 = s0 + 64;
        bytes[s1 + 4..s1 + 8].copy_from_slice(&11u32.to_le_bytes());
        bytes[s1 + 24..s1 + 32].copy_from_slice(&(sym_off as u64).to_le_bytes());
        bytes[s1 + 32..s1 + 40].copy_from_slice(&(4u64 * 24).to_le_bytes());
        bytes[s1 + 40..s1 + 44].copy_from_slice(&2u32.to_le_bytes());
        bytes[s1 + 56..s1 + 64].copy_from_slice(&24u64.to_le_bytes());

        // Section 2 at 0x180: SHT_STRTAB (3).
        let s2 = s0 + 128;
        bytes[s2 + 4..s2 + 8].copy_from_slice(&3u32.to_le_bytes());
        bytes[s2 + 24..s2 + 32].copy_from_slice(&(str_off as u64).to_le_bytes());
        bytes[s2 + 32..s2 + 40].copy_from_slice(&(strtab.len() as u64).to_le_bytes());

        bytes
    }
}
