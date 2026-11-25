//! macOS `dyld_v1` shared cache reader (`dyld_shared_cache` + subcaches + `.symbols`).
//!
//! Since macOS 11 Big Sur, system dylibs (`/usr/lib/system/libsystem_kernel.dylib`,
//! `/usr/lib/libSystem.B.dylib`, CoreFoundation, ...) no longer exist as standalone
//! Mach-O files on disk. Their headers, `__TEXT` (`__text`, `__unwind_info`,
//! `__eh_frame`), and shared `__LINKEDIT` live inside a multi-file `dyld_v1`
//! shared cache under `/System/Volumes/Preboot/Cryptexes/OS/System/Library/dyld/`
//! (or `/System/Library/dyld/`).
//!
//! This module opens only the files needed via positioned reads (`pread` /
//! `seek_read`), never maps or allocates the multi-gigabyte cache or its
//! ~150 MiB shared string table, skips `"<redacted>"` placeholders, walks each
//! dylib's `LC_DYLD_EXPORTS_TRIE` and optional `.symbols` local-symbol table,
//! and extracts Compact Unwind / `__eh_frame` sections for `framehop`.

#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyprof_core::Runtime;

use crate::macho::{
    Header64, ImageSymbols, LC_SEGMENT_64, LoadCommands, MAX_EH_FRAME_BYTES, MAX_UNWIND_INFO_BYTES,
    MachCpu, MachoUnwind, N_SECT, N_STAB, N_TYPE, S_ZEROFILL, find_symtab, segname,
};
use crate::symbol::{self, Presented};
use crate::walk::SectionBytes;

const MIN_HEADER_SIZE: u32 = 0x68;
const MAX_HEADER_SIZE: u32 = 0x4000;
const MIN_HEADER_SUBCACHES_V1: u32 = 0x1c8;
const MIN_HEADER_SUBCACHES_V2: u32 = 0x1d0;

const MAX_MAPPINGS: u32 = 4096;
const MAX_SUBCACHES: u32 = 256;
const MAX_IMAGES: u32 = 16_384;
const MAX_LOCAL_ENTRIES: u32 = 32_768;
const MAX_NSYMS: u32 = 2_000_000;
const MAX_EXPORT_TRIE_BYTES: u64 = 16 * 1024 * 1024;
const MAX_TRIE_NODES: usize = 200_000;

const LC_DYLD_INFO: u32 = 0x22;
const LC_DYLD_INFO_ONLY: u32 = 0x8000_0022;
const LC_DYLD_EXPORTS_TRIE: u32 = 0x8000_0033;

const EXPORT_SYMBOL_FLAGS_KIND_MASK: u64 = 0x03;
const EXPORT_SYMBOL_FLAGS_KIND_REGULAR: u64 = 0x00;
const EXPORT_SYMBOL_FLAGS_REEXPORT: u64 = 0x08;
const EXPORT_SYMBOL_FLAGS_STUB_AND_RESOLVER: u64 = 0x10;

#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn dyld_shared_cache_file_path() -> *const std::ffi::c_char;
}

enum CacheSource {
    File(File, u64),
    #[cfg(test)]
    Memory(Vec<u8>),
}

impl CacheSource {
    fn open(path: &Path) -> Option<Self> {
        let file = File::open(path).ok()?;
        let len = file.metadata().ok()?.len();
        Some(Self::File(file, len))
    }

    fn len(&self) -> u64 {
        match self {
            Self::File(_, len) => *len,
            #[cfg(test)]
            Self::Memory(bytes) => bytes.len() as u64,
        }
    }

    fn read_exact_at(&self, offset: u64, buf: &mut [u8]) -> bool {
        let Some(end) = offset.checked_add(buf.len() as u64) else {
            return false;
        };
        if end > self.len() {
            return false;
        }
        match self {
            Self::File(file, _) => read_file_exact_at(file, offset, buf),
            #[cfg(test)]
            Self::Memory(bytes) => {
                let start = offset as usize;
                buf.copy_from_slice(&bytes[start..start + buf.len()]);
                true
            }
        }
    }

    fn read_vec_at(&self, offset: u64, size: u64, cap: u64) -> Option<Vec<u8>> {
        if size == 0 || size > cap {
            return None;
        }
        let len = usize::try_from(size).ok()?;
        let mut buf = vec![0u8; len];
        if !self.read_exact_at(offset, &mut buf) {
            return None;
        }
        Some(buf)
    }
}

#[cfg(unix)]
fn read_file_exact_at(file: &File, mut offset: u64, mut buf: &mut [u8]) -> bool {
    use std::os::unix::fs::FileExt as _;
    while !buf.is_empty() {
        match file.read_at(buf, offset) {
            Ok(0) => return false,
            Ok(n) => {
                buf = &mut buf[n..];
                offset = offset.saturating_add(n as u64);
            }
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return false,
        }
    }
    true
}

#[cfg(windows)]
fn read_file_exact_at(file: &File, mut offset: u64, mut buf: &mut [u8]) -> bool {
    use std::os::windows::fs::FileExt as _;
    while !buf.is_empty() {
        match file.seek_read(buf, offset) {
            Ok(0) => return false,
            Ok(n) => {
                buf = &mut buf[n..];
                offset = offset.saturating_add(n as u64);
            }
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return false,
        }
    }
    true
}

#[derive(Clone, Copy)]
struct CacheMapping {
    vmaddr: u64,
    vmsize: u64,
    file_idx: usize,
    file_offset: u64,
}

#[derive(Clone, Copy)]
struct CachedImageEntry {
    header_vmaddr: u64,
}

struct DeferredLocalSymbols {
    main_path: PathBuf,
    local_symbols_offset: u64,
    local_symbols_size: u64,
    symbol_file_uuid: [u8; 16],
    uses_64bit_dylib_offset: bool,
}

struct LocalSymbols {
    source: CacheSource,
    nlist_offset: u64,
    nlist_count: u32,
    strings_offset: u64,
    strings_size: u64,
    uses_64bit_dylib_offset: bool,
    by_dylib_key: HashMap<u64, (u32, u32)>,
}

/// Reader for macOS's `dyld_v1` shared cache and its subcaches.
pub(crate) struct DyldSharedCache {
    files: Vec<CacheSource>,
    mappings: Vec<CacheMapping>,
    shared_region_start: u64,
    images_by_path: HashMap<String, CachedImageEntry>,
    local_spec: Option<DeferredLocalSymbols>,
    local_symbols: OnceLock<Option<LocalSymbols>>,
}

struct ParsedCacheHeader {
    mapping_offset: u32,
    mapping_count: u32,
    images_offset: u32,
    images_count: u32,
    local_symbols_offset: u64,
    local_symbols_size: u64,
    uuid: [u8; 16],
    shared_region_start: u64,
    sub_cache_array_offset: u32,
    sub_cache_array_count: u32,
    symbol_file_uuid: [u8; 16],
}

struct CachedMachoCommands {
    hdr_file_off: u64,
    header_vmaddr: u64,
    ncmds: usize,
    commands: Vec<u8>,
    text_vmaddr: u64,
    text_vmsize: u64,
    linkedit_vmaddr: Option<u64>,
    export_loc: Option<(u64, u64)>,
}

impl DyldSharedCache {
    /// Process-wide singleton for the active host `dyld_shared_cache` on macOS.
    #[cfg(target_os = "macos")]
    pub(crate) fn host(cpu: MachCpu) -> Option<&'static Self> {
        static HOST_CACHE: OnceLock<Option<DyldSharedCache>> = OnceLock::new();
        HOST_CACHE.get_or_init(|| Self::open_host(cpu)).as_ref()
    }

    /// Open the active host `dyld_shared_cache` on macOS.
    #[cfg(target_os = "macos")]
    pub(crate) fn open_host(cpu: MachCpu) -> Option<Self> {
        for candidate in candidate_cache_paths(cpu) {
            if let Some(cache) = Self::open_path(&candidate, cpu) {
                return Some(cache);
            }
        }
        None
    }

    /// Open a `dyld_shared_cache` rooted at `main_path`.
    pub(crate) fn open_path(main_path: &Path, cpu: MachCpu) -> Option<Self> {
        let main_source = CacheSource::open(main_path)?;
        let main_hdr = parse_cache_header(&main_source, Some(cpu))?;
        let mut files = vec![main_source];
        let mut mappings = read_mappings(&files[0], 0, &main_hdr)?;

        if main_hdr.mapping_offset >= MIN_HEADER_SUBCACHES_V1 && main_hdr.sub_cache_array_count > 0
        {
            if main_hdr.sub_cache_array_count > MAX_SUBCACHES {
                return None;
            }
            let subcaches = read_subcache_entries(&files[0], &main_hdr)?;
            for (expected_uuid, suffix) in subcaches {
                let sub_path = append_suffix(main_path, &suffix);
                let Ok(Some((sub_src, sub_hdr))) = open_verified_subcache(&sub_path, expected_uuid)
                else {
                    continue;
                };
                let file_idx = files.len();
                if let Some(sub_maps) = read_mappings(&sub_src, file_idx, &sub_hdr) {
                    mappings.extend(sub_maps);
                    files.push(sub_src);
                }
            }
        }

        mappings.sort_by_key(|m| m.vmaddr);
        let shared_region_start = if main_hdr.shared_region_start != 0 {
            main_hdr.shared_region_start
        } else {
            mappings.first().map(|m| m.vmaddr).unwrap_or(0)
        };
        let images_by_path = read_image_index(&files[0], &main_hdr)?;
        let local_spec = Some(DeferredLocalSymbols {
            main_path: main_path.to_path_buf(),
            local_symbols_offset: main_hdr.local_symbols_offset,
            local_symbols_size: main_hdr.local_symbols_size,
            symbol_file_uuid: main_hdr.symbol_file_uuid,
            uses_64bit_dylib_offset: main_hdr.mapping_offset >= MIN_HEADER_SUBCACHES_V1,
        });

        Some(Self {
            files,
            mappings,
            shared_region_start,
            images_by_path,
            local_spec,
            local_symbols: OnceLock::new(),
        })
    }

    #[cfg(test)]
    fn from_memory_parts(
        main_bytes: Vec<u8>,
        subcaches: Vec<(String, Vec<u8>)>,
        symbols_bytes: Option<Vec<u8>>,
        cpu: MachCpu,
    ) -> Option<Self> {
        let main_source = CacheSource::Memory(main_bytes);
        let main_hdr = parse_cache_header(&main_source, Some(cpu))?;
        let mut files = vec![main_source];
        let mut mappings = read_mappings(&files[0], 0, &main_hdr)?;
        let mut by_suffix: HashMap<String, Vec<u8>> = subcaches.into_iter().collect();
        if main_hdr.mapping_offset >= MIN_HEADER_SUBCACHES_V1 && main_hdr.sub_cache_array_count > 0
        {
            let entries = read_subcache_entries(&files[0], &main_hdr)?;
            for (expected_uuid, suffix) in entries {
                let Some(bytes) = by_suffix.remove(&suffix) else {
                    continue;
                };
                let sub_src = CacheSource::Memory(bytes);
                let Some(sub_hdr) = parse_cache_header(&sub_src, None) else {
                    continue;
                };
                if sub_hdr.uuid != expected_uuid {
                    continue;
                }
                let idx = files.len();
                if let Some(sub_maps) = read_mappings(&sub_src, idx, &sub_hdr) {
                    mappings.extend(sub_maps);
                    files.push(sub_src);
                }
            }
        }
        mappings.sort_by_key(|m| m.vmaddr);
        let shared_region_start = if main_hdr.shared_region_start != 0 {
            main_hdr.shared_region_start
        } else {
            mappings.first().map(|m| m.vmaddr).unwrap_or(0)
        };
        let images_by_path = read_image_index(&files[0], &main_hdr)?;
        let parsed_local = symbols_bytes.and_then(|bytes| {
            let sym_src = CacheSource::Memory(bytes);
            let sym_hdr = parse_cache_header(&sym_src, None)?;
            if main_hdr.symbol_file_uuid == [0; 16] || sym_hdr.uuid != main_hdr.symbol_file_uuid {
                return None;
            }
            parse_local_symbols(
                sym_src,
                sym_hdr.local_symbols_offset,
                sym_hdr.local_symbols_size,
                main_hdr.mapping_offset >= MIN_HEADER_SUBCACHES_V1,
            )
        });
        let local_symbols = OnceLock::new();
        let _ = local_symbols.set(parsed_local);
        Some(Self {
            files,
            mappings,
            shared_region_start,
            images_by_path,
            local_spec: None,
            local_symbols,
        })
    }

    /// Read only `__TEXT` bounds and Compact Unwind / `__eh_frame` sections for
    /// `dylib_path`, deferring symbol table and `.symbols` parsing until needed.
    pub(crate) fn load_unwind(
        &self,
        dylib_path: &str,
        runtime_load_addr: u64,
        module: &str,
    ) -> Option<ImageSymbols> {
        let meta = self.read_cached_macho_commands(dylib_path)?;
        let unwind = self.read_cached_unwind(meta.text_vmaddr, &meta.commands, meta.ncmds);
        ImageSymbols::from_cached(
            runtime_load_addr,
            meta.text_vmaddr,
            meta.text_vmsize,
            module.to_string(),
            Vec::new(),
            unwind,
        )
    }

    /// Populate symbols (`LC_SYMTAB`, `LC_DYLD_EXPORTS_TRIE`, and `.symbols`)
    /// onto an [`ImageSymbols`] previously created via [`Self::load_unwind`].
    pub(crate) fn populate_symbols(&self, dylib_path: &str, image: &mut ImageSymbols) {
        let Some(meta) = self.read_cached_macho_commands(dylib_path) else {
            return;
        };
        let mut symbols = Vec::new();
        if let Some(le_vmaddr) = meta.linkedit_vmaddr
            && let Some((le_file_idx, _, _)) = self.resolve_vmaddr(le_vmaddr)
            && let Some(le_file) = self.files.get(le_file_idx)
        {
            if let Some(symtab) = find_symtab(&meta.commands, meta.ncmds) {
                read_nlist_symbols(
                    le_file,
                    symtab.symoff,
                    symtab.nsyms,
                    symtab.stroff,
                    u64::from(symtab.strsize),
                    &mut symbols,
                );
            }
            if let Some((exp_off, exp_size)) = meta.export_loc
                && let Some(trie) = le_file.read_vec_at(exp_off, exp_size, MAX_EXPORT_TRIE_BYTES)
            {
                parse_exports_trie(&trie, meta.text_vmaddr, meta.text_vmsize, &mut symbols);
            }
        }

        if let Some(local) = self.local_symbols() {
            let key = if local.uses_64bit_dylib_offset {
                meta.header_vmaddr.checked_sub(self.shared_region_start)
            } else {
                Some(meta.hdr_file_off)
            };
            if let Some(key) = key
                && let Some(&(start_idx, count)) = local.by_dylib_key.get(&key)
                && start_idx.saturating_add(count) <= local.nlist_count
            {
                let sym_off = local
                    .nlist_offset
                    .saturating_add(u64::from(start_idx).saturating_mul(16));
                read_nlist_symbols(
                    &local.source,
                    sym_off,
                    count,
                    local.strings_offset,
                    local.strings_size,
                    &mut symbols,
                );
            }
        }
        image.set_unslid_symbols(symbols);
    }

    /// Parse symbols and Compact Unwind / `__eh_frame` sections for `dylib_path`
    /// loaded at `runtime_load_addr` in the target task.
    #[cfg(test)]
    pub(crate) fn load_image(
        &self,
        dylib_path: &str,
        runtime_load_addr: u64,
        module: &str,
    ) -> Option<ImageSymbols> {
        let mut image = self.load_unwind(dylib_path, runtime_load_addr, module)?;
        self.populate_symbols(dylib_path, &mut image);
        Some(image)
    }

    fn local_symbols(&self) -> Option<&LocalSymbols> {
        self.local_symbols
            .get_or_init(|| {
                let spec = self.local_spec.as_ref()?;
                open_local_symbols(spec)
            })
            .as_ref()
    }

    fn read_cached_macho_commands(&self, dylib_path: &str) -> Option<CachedMachoCommands> {
        let entry = self.images_by_path.get(dylib_path)?;
        let (hdr_file_idx, hdr_file_off, hdr_avail) = self.resolve_vmaddr(entry.header_vmaddr)?;
        if hdr_avail < 32 {
            return None;
        }
        let hdr_file = self.files.get(hdr_file_idx)?;
        let mut header_buf = [0u8; 32];
        if !hdr_file.read_exact_at(hdr_file_off, &mut header_buf) {
            return None;
        }
        let header = Header64::read(&header_buf)?;
        if header.sizeofcmds == 0
            || header.sizeofcmds > 1024 * 1024
            || (32 + header.sizeofcmds as u64) > hdr_avail
        {
            return None;
        }
        let mut commands = vec![0u8; header.sizeofcmds];
        if !hdr_file.read_exact_at(hdr_file_off + 32, &mut commands) {
            return None;
        }

        let mut text_vmaddr = None;
        let mut text_vmsize = 0u64;
        let mut linkedit_vmaddr = None;
        let mut export_loc = None;

        for cmd in LoadCommands::new(&commands, header.ncmds) {
            if cmd.cmd == LC_SEGMENT_64 && cmd.data.len() >= 32 {
                let name = segname(&cmd.data[0..16]);
                let vmaddr = u64::from_le_bytes(cmd.data[16..24].try_into().ok()?);
                let vmsize = u64::from_le_bytes(cmd.data[24..32].try_into().ok()?);
                match name {
                    "__TEXT" => {
                        text_vmaddr = Some(vmaddr);
                        text_vmsize = vmsize;
                    }
                    "__LINKEDIT" => {
                        linkedit_vmaddr = Some(vmaddr);
                    }
                    _ => {}
                }
            } else if cmd.cmd == LC_DYLD_EXPORTS_TRIE && cmd.data.len() >= 8 {
                let off = u64::from(u32::from_le_bytes(cmd.data[0..4].try_into().ok()?));
                let size = u64::from(u32::from_le_bytes(cmd.data[4..8].try_into().ok()?));
                if size > 0 {
                    export_loc = Some((off, size));
                }
            } else if (cmd.cmd == LC_DYLD_INFO || cmd.cmd == LC_DYLD_INFO_ONLY)
                && cmd.data.len() >= 40
                && export_loc.is_none()
            {
                let off = u64::from(u32::from_le_bytes(cmd.data[32..36].try_into().ok()?));
                let size = u64::from(u32::from_le_bytes(cmd.data[36..40].try_into().ok()?));
                if size > 0 {
                    export_loc = Some((off, size));
                }
            }
        }

        let text_vmaddr = text_vmaddr?;
        if text_vmsize == 0 {
            return None;
        }
        Some(CachedMachoCommands {
            hdr_file_off,
            header_vmaddr: entry.header_vmaddr,
            ncmds: header.ncmds,
            commands,
            text_vmaddr,
            text_vmsize,
            linkedit_vmaddr,
            export_loc,
        })
    }

    fn read_cached_unwind(&self, base_svma: u64, commands: &[u8], ncmds: usize) -> MachoUnwind {
        let mut unwind = MachoUnwind {
            base_svma,
            ..Default::default()
        };
        let mut unwind_range = None;
        let mut eh_range = None;

        for cmd in LoadCommands::new(commands, ncmds) {
            if cmd.cmd != LC_SEGMENT_64 || cmd.data.len() < 64 {
                continue;
            }
            let Ok(nsects_bytes) = cmd.data[56..60].try_into() else {
                continue;
            };
            let nsects = u32::from_le_bytes(nsects_bytes) as usize;
            let Some(needed) = 64usize.checked_add(nsects.saturating_mul(80)) else {
                continue;
            };
            if cmd.data.len() < needed {
                continue;
            }
            for idx in 0..nsects {
                let sec = &cmd.data[64 + idx * 80..64 + (idx + 1) * 80];
                let sect = segname(&sec[0..16]);
                let seg = segname(&sec[16..32]);
                let addr = u64::from_le_bytes(sec[32..40].try_into().unwrap());
                let size = u64::from_le_bytes(sec[40..48].try_into().unwrap());
                let flags = u32::from_le_bytes(sec[64..68].try_into().unwrap());
                if size == 0 || (flags & 0xff) == S_ZEROFILL {
                    continue;
                }
                let range = addr..addr.saturating_add(size);
                match (seg, sect) {
                    ("__TEXT", "__text") if unwind.text_svma.is_none() => {
                        unwind.text_svma = Some(range);
                    }
                    ("__TEXT", "__stubs") if unwind.stubs_svma.is_none() => {
                        unwind.stubs_svma = Some(range);
                    }
                    ("__TEXT", "__stub_helper") if unwind.stub_helper_svma.is_none() => {
                        unwind.stub_helper_svma = Some(range);
                    }
                    ("__TEXT", "__unwind_info") if unwind_range.is_none() => {
                        unwind_range = Some((addr, size));
                    }
                    ("__TEXT", "__eh_frame") if eh_range.is_none() => {
                        unwind.eh_frame_svma = Some(range);
                        eh_range = Some((addr, size));
                    }
                    (_, "__got") if unwind.got_svma.is_none() => {
                        unwind.got_svma = Some(range);
                    }
                    _ => {}
                }
            }
        }

        if let Some((addr, size)) = unwind_range {
            unwind.unwind_info = self
                .read_vm_bytes(addr, size, MAX_UNWIND_INFO_BYTES)
                .map(SectionBytes::new);
        }
        if let Some((addr, size)) = eh_range {
            unwind.eh_frame = self
                .read_vm_bytes(addr, size, MAX_EH_FRAME_BYTES)
                .map(SectionBytes::new);
        }
        unwind
    }

    fn resolve_vmaddr(&self, vmaddr: u64) -> Option<(usize, u64, u64)> {
        let idx = self.mappings.partition_point(|m| m.vmaddr <= vmaddr);
        let map = self.mappings.get(idx.checked_sub(1)?)?;
        let delta = vmaddr.checked_sub(map.vmaddr)?;
        if delta >= map.vmsize {
            return None;
        }
        Some((
            map.file_idx,
            map.file_offset.checked_add(delta)?,
            map.vmsize - delta,
        ))
    }

    fn read_vm_bytes(&self, vmaddr: u64, size: u64, cap: u64) -> Option<Vec<u8>> {
        if size == 0 || size > cap {
            return None;
        }
        let (file_idx, file_off, avail) = self.resolve_vmaddr(vmaddr)?;
        if size > avail {
            return None;
        }
        self.files.get(file_idx)?.read_vec_at(file_off, size, cap)
    }
}

#[cfg(target_os = "macos")]
fn candidate_cache_paths(cpu: MachCpu) -> Vec<PathBuf> {
    let mut out = Vec::new();
    // SAFETY: `dyld_shared_cache_file_path` is exported by `libSystem` since
    // macOS 11 and returns either null or a static NUL-terminated path to the
    // shared cache mapped into the current process.
    let raw = unsafe { dyld_shared_cache_file_path() };
    if !raw.is_null()
        && let Ok(text) = unsafe { std::ffi::CStr::from_ptr(raw) }.to_str()
        && !text.is_empty()
    {
        out.push(PathBuf::from(text));
    }
    let dirs = [
        "/System/Volumes/Preboot/Cryptexes/OS/System/Library/dyld",
        "/System/Cryptexes/OS/System/Library/dyld",
        "/System/Library/dyld",
    ];
    let names: &[&str] = match cpu {
        MachCpu::Arm64 => &["dyld_shared_cache_arm64e", "dyld_shared_cache_arm64"],
        MachCpu::X86_64 => &["dyld_shared_cache_x86_64h", "dyld_shared_cache_x86_64"],
    };
    for dir in dirs {
        for name in names {
            let path = Path::new(dir).join(name);
            if !out.contains(&path) {
                out.push(path);
            }
        }
    }
    out
}

fn parse_cache_header(
    source: &CacheSource,
    expected_cpu: Option<MachCpu>,
) -> Option<ParsedCacheHeader> {
    let mut probe = [0u8; 24];
    if !source.read_exact_at(0, &mut probe) {
        return None;
    }
    let magic = &probe[0..16];
    if !magic.starts_with(b"dyld_v1") {
        return None;
    }
    if let Some(cpu) = expected_cpu {
        let matches = match cpu {
            MachCpu::Arm64 => magic == b"dyld_v1  arm64e\0" || magic == b"dyld_v1   arm64\0",
            MachCpu::X86_64 => magic == b"dyld_v1 x86_64h\0" || magic == b"dyld_v1  x86_64\0",
        };
        if !matches {
            return None;
        }
    }
    let mapping_offset = u32::from_le_bytes(probe[0x10..0x14].try_into().ok()?);
    let mapping_count = u32::from_le_bytes(probe[0x14..0x18].try_into().ok()?);
    if !(MIN_HEADER_SIZE..=MAX_HEADER_SIZE).contains(&mapping_offset)
        || mapping_count > MAX_MAPPINGS
    {
        return None;
    }
    let hdr = source.read_vec_at(0, u64::from(mapping_offset), u64::from(MAX_HEADER_SIZE))?;
    let images_offset_old = u32::from_le_bytes(hdr[0x18..0x1c].try_into().ok()?);
    let images_count_old = u32::from_le_bytes(hdr[0x1c..0x20].try_into().ok()?);
    let local_symbols_offset = u64::from_le_bytes(hdr[0x48..0x50].try_into().ok()?);
    let local_symbols_size = u64::from_le_bytes(hdr[0x50..0x58].try_into().ok()?);
    let uuid: [u8; 16] = hdr[0x58..0x68].try_into().ok()?;
    let shared_region_start = if mapping_offset >= 0xe8 {
        u64::from_le_bytes(hdr[0xe0..0xe8].try_into().ok()?)
    } else {
        0
    };
    let (
        sub_cache_array_offset,
        sub_cache_array_count,
        symbol_file_uuid,
        images_offset,
        images_count,
    ) = if mapping_offset >= MIN_HEADER_SUBCACHES_V1 {
        let sub_off = u32::from_le_bytes(hdr[0x188..0x18c].try_into().ok()?);
        let sub_cnt = u32::from_le_bytes(hdr[0x18c..0x190].try_into().ok()?);
        let sym_uuid: [u8; 16] = hdr[0x190..0x1a0].try_into().ok()?;
        let img_off = u32::from_le_bytes(hdr[0x1c0..0x1c4].try_into().ok()?);
        let img_cnt = u32::from_le_bytes(hdr[0x1c4..0x1c8].try_into().ok()?);
        (sub_off, sub_cnt, sym_uuid, img_off, img_cnt)
    } else {
        (0, 0, [0u8; 16], images_offset_old, images_count_old)
    };
    Some(ParsedCacheHeader {
        mapping_offset,
        mapping_count,
        images_offset,
        images_count,
        local_symbols_offset,
        local_symbols_size,
        uuid,
        shared_region_start,
        sub_cache_array_offset,
        sub_cache_array_count,
        symbol_file_uuid,
    })
}

fn open_verified_subcache(
    path: &Path,
    expected_uuid: [u8; 16],
) -> Result<Option<(CacheSource, ParsedCacheHeader)>, ()> {
    let Some(src) = CacheSource::open(path) else {
        return Ok(None);
    };
    let Some(hdr) = parse_cache_header(&src, None) else {
        return Ok(None);
    };
    if hdr.uuid != expected_uuid {
        return Ok(None);
    }
    Ok(Some((src, hdr)))
}

fn read_mappings(
    source: &CacheSource,
    file_idx: usize,
    hdr: &ParsedCacheHeader,
) -> Option<Vec<CacheMapping>> {
    if hdr.mapping_count == 0 {
        return Some(Vec::new());
    }
    let total = u64::from(hdr.mapping_count).checked_mul(32)?;
    let raw = source.read_vec_at(
        u64::from(hdr.mapping_offset),
        total,
        u64::from(MAX_MAPPINGS) * 32,
    )?;
    let mut out = Vec::with_capacity(hdr.mapping_count as usize);
    let (rows, _) = raw.as_chunks::<32>();
    for row in rows {
        let vmaddr = u64::from_le_bytes(row[0..8].try_into().ok()?);
        let vmsize = u64::from_le_bytes(row[8..16].try_into().ok()?);
        let file_offset = u64::from_le_bytes(row[16..24].try_into().ok()?);
        if vmsize == 0 || file_offset.saturating_add(vmsize) > source.len() {
            continue;
        }
        out.push(CacheMapping {
            vmaddr,
            vmsize,
            file_idx,
            file_offset,
        });
    }
    Some(out)
}

fn read_subcache_entries(
    main_source: &CacheSource,
    hdr: &ParsedCacheHeader,
) -> Option<Vec<([u8; 16], String)>> {
    let count = hdr.sub_cache_array_count as usize;
    if hdr.mapping_offset >= MIN_HEADER_SUBCACHES_V2 {
        let bytes = main_source.read_vec_at(
            u64::from(hdr.sub_cache_array_offset),
            (count as u64).checked_mul(56)?,
            u64::from(MAX_SUBCACHES) * 56,
        )?;
        let mut out = Vec::with_capacity(count);
        let (rows, _) = bytes.as_chunks::<56>();
        for row in rows {
            let uuid: [u8; 16] = row[0..16].try_into().ok()?;
            let suffix = segname(&row[24..56]).to_string();
            if !suffix.is_empty() {
                out.push((uuid, suffix));
            }
        }
        Some(out)
    } else {
        let bytes = main_source.read_vec_at(
            u64::from(hdr.sub_cache_array_offset),
            (count as u64).checked_mul(24)?,
            u64::from(MAX_SUBCACHES) * 24,
        )?;
        let mut out = Vec::with_capacity(count);
        let (rows, _) = bytes.as_chunks::<24>();
        for (idx, row) in rows.iter().enumerate() {
            let uuid: [u8; 16] = row[0..16].try_into().ok()?;
            out.push((uuid, format!(".{}", idx + 1)));
        }
        Some(out)
    }
}

fn read_image_index(
    main_source: &CacheSource,
    hdr: &ParsedCacheHeader,
) -> Option<HashMap<String, CachedImageEntry>> {
    if hdr.images_count == 0 || hdr.images_count > MAX_IMAGES {
        return None;
    }
    let total = u64::from(hdr.images_count).checked_mul(32)?;
    let raw = main_source.read_vec_at(
        u64::from(hdr.images_offset),
        total,
        u64::from(MAX_IMAGES) * 32,
    )?;
    let (rows, _) = raw.as_chunks::<32>();
    let mut pending = Vec::with_capacity(rows.len());
    for row in rows {
        let header_vmaddr = u64::from_le_bytes(row[0..8].try_into().ok()?);
        let path_off = u64::from(u32::from_le_bytes(row[24..28].try_into().ok()?));
        if header_vmaddr != 0 && path_off != 0 {
            pending.push((path_off, header_vmaddr));
        }
    }
    pending.sort_unstable_by_key(|&(path_off, _)| path_off);
    let mut map = HashMap::with_capacity(pending.len());
    let mut str_reader = PagedStringReader::new(main_source, 0, main_source.len());
    for (path_off, header_vmaddr) in pending {
        if let Some(path) = str_reader.read_cstr(path_off, 1024) {
            map.insert(path, CachedImageEntry { header_vmaddr });
        }
    }
    Some(map)
}

fn open_local_symbols(spec: &DeferredLocalSymbols) -> Option<LocalSymbols> {
    if spec.symbol_file_uuid != [0; 16] {
        let sym_path = append_suffix(&spec.main_path, ".symbols");
        if let Ok(Some((sym_src, sym_hdr))) =
            open_verified_subcache(&sym_path, spec.symbol_file_uuid)
            && let Some(parsed) = parse_local_symbols(
                sym_src,
                sym_hdr.local_symbols_offset,
                sym_hdr.local_symbols_size,
                spec.uses_64bit_dylib_offset,
            )
        {
            return Some(parsed);
        }
    }
    if spec.local_symbols_offset != 0
        && spec.local_symbols_size >= 24
        && let Some(dup_src) = CacheSource::open(&spec.main_path)
    {
        return parse_local_symbols(
            dup_src,
            spec.local_symbols_offset,
            spec.local_symbols_size,
            spec.uses_64bit_dylib_offset,
        );
    }
    None
}

fn parse_local_symbols(
    source: CacheSource,
    local_off: u64,
    local_size: u64,
    uses_64bit_dylib_offset: bool,
) -> Option<LocalSymbols> {
    if local_off == 0 || local_size < 24 || local_off.checked_add(local_size)? > source.len() {
        return None;
    }
    let mut info = [0u8; 24];
    if !source.read_exact_at(local_off, &mut info) {
        return None;
    }
    let nlist_rel = u64::from(u32::from_le_bytes(info[0..4].try_into().ok()?));
    let nlist_count = u32::from_le_bytes(info[4..8].try_into().ok()?);
    let strings_rel = u64::from(u32::from_le_bytes(info[8..12].try_into().ok()?));
    let strings_size = u64::from(u32::from_le_bytes(info[12..16].try_into().ok()?));
    let entries_rel = u64::from(u32::from_le_bytes(info[16..20].try_into().ok()?));
    let entries_count = u32::from_le_bytes(info[20..24].try_into().ok()?);
    if entries_count == 0 || entries_count > MAX_LOCAL_ENTRIES {
        return None;
    }
    let nlist_offset = local_off.checked_add(nlist_rel)?;
    let strings_offset = local_off.checked_add(strings_rel)?;
    let entries_offset = local_off.checked_add(entries_rel)?;
    if nlist_offset.checked_add(u64::from(nlist_count).checked_mul(16)?)? > source.len()
        || strings_offset.checked_add(strings_size)? > source.len()
    {
        return None;
    }

    let mut by_dylib_key = HashMap::with_capacity(entries_count as usize);
    if uses_64bit_dylib_offset {
        let total = u64::from(entries_count).checked_mul(16)?;
        let raw = source.read_vec_at(entries_offset, total, u64::from(MAX_LOCAL_ENTRIES) * 16)?;
        let (rows, _) = raw.as_chunks::<16>();
        for row in rows {
            let dylib_off = u64::from_le_bytes(row[0..8].try_into().ok()?);
            let start = u32::from_le_bytes(row[8..12].try_into().ok()?);
            let count = u32::from_le_bytes(row[12..16].try_into().ok()?);
            by_dylib_key.insert(dylib_off, (start, count));
        }
    } else {
        let total = u64::from(entries_count).checked_mul(12)?;
        let raw = source.read_vec_at(entries_offset, total, u64::from(MAX_LOCAL_ENTRIES) * 12)?;
        let (rows, _) = raw.as_chunks::<12>();
        for row in rows {
            let dylib_off = u64::from(u32::from_le_bytes(row[0..4].try_into().ok()?));
            let start = u32::from_le_bytes(row[4..8].try_into().ok()?);
            let count = u32::from_le_bytes(row[8..12].try_into().ok()?);
            by_dylib_key.insert(dylib_off, (start, count));
        }
    }

    Some(LocalSymbols {
        source,
        nlist_offset,
        nlist_count,
        strings_offset,
        strings_size,
        uses_64bit_dylib_offset,
        by_dylib_key,
    })
}

fn read_nlist_symbols(
    source: &CacheSource,
    symoff: u64,
    nsyms: u32,
    stroff: u64,
    strsize: u64,
    out: &mut Vec<(u64, String, Runtime)>,
) {
    if nsyms == 0 || nsyms > MAX_NSYMS || strsize == 0 {
        return;
    }
    let Some(sym_bytes) = u64::from(nsyms).checked_mul(16) else {
        return;
    };
    let Some(raw) = source.read_vec_at(symoff, sym_bytes, u64::from(MAX_NSYMS) * 16) else {
        return;
    };
    let (entries, _) = raw.as_chunks::<16>();
    let mut pending = Vec::with_capacity(entries.len());
    for entry in entries {
        let n_strx = u64::from(u32::from_le_bytes(entry[0..4].try_into().unwrap()));
        let n_type = entry[4];
        let n_sect = entry[5];
        let n_value = u64::from_le_bytes(entry[8..16].try_into().unwrap());
        if n_sect == 0 || n_value == 0 || n_type & N_STAB != 0 || n_type & N_TYPE != N_SECT {
            continue;
        }
        pending.push((n_strx, n_value));
    }
    pending.sort_unstable_by_key(|&(n_strx, _)| n_strx);
    let mut str_reader = PagedStringReader::new(source, stroff, strsize);
    for (n_strx, n_value) in pending {
        let Some(raw_name) = str_reader.read_cstr(n_strx, 2048) else {
            continue;
        };
        if raw_name == "<redacted>" {
            continue;
        }
        let Presented { name, runtime, .. } = symbol::present_macho_symbol(&raw_name);
        out.push((n_value, name, runtime));
    }
}

fn parse_exports_trie(
    trie: &[u8],
    text_vmaddr: u64,
    text_vmsize: u64,
    out: &mut Vec<(u64, String, Runtime)>,
) {
    if trie.is_empty() {
        return;
    }
    let mut visited = HashSet::new();
    let mut stack = vec![(0usize, String::new())];
    while let Some((node_off, prefix)) = stack.pop() {
        if visited.len() >= MAX_TRIE_NODES || !visited.insert(node_off) {
            continue;
        }
        let mut cursor = node_off;
        let Some(terminal_size) = read_uleb128(trie, &mut cursor) else {
            continue;
        };
        let Ok(terminal_len) = usize::try_from(terminal_size) else {
            continue;
        };
        let Some(children_pos) = cursor.checked_add(terminal_len) else {
            continue;
        };
        if children_pos > trie.len() {
            continue;
        }
        if terminal_len > 0 && !prefix.is_empty() && prefix != "<redacted>" {
            let mut term_cur = cursor;
            if let Some(flags) = read_uleb128(trie, &mut term_cur)
                && (flags & EXPORT_SYMBOL_FLAGS_REEXPORT) == 0
                && (flags & EXPORT_SYMBOL_FLAGS_KIND_MASK) == EXPORT_SYMBOL_FLAGS_KIND_REGULAR
                && let Some(sym_off) = read_uleb128(trie, &mut term_cur)
            {
                let _ = flags & EXPORT_SYMBOL_FLAGS_STUB_AND_RESOLVER;
                if sym_off < text_vmsize {
                    let addr = text_vmaddr.wrapping_add(sym_off);
                    let Presented { name, runtime, .. } = symbol::present_macho_symbol(&prefix);
                    out.push((addr, name, runtime));
                }
            }
        }
        let Some(&child_count) = trie.get(children_pos) else {
            continue;
        };
        let mut edge_cur = children_pos + 1;
        for _ in 0..child_count {
            let Some(edge_label) = read_cstr_slice(trie, &mut edge_cur) else {
                break;
            };
            let Some(child_off_u64) = read_uleb128(trie, &mut edge_cur) else {
                break;
            };
            let Ok(child_off) = usize::try_from(child_off_u64) else {
                break;
            };
            if child_off < trie.len() && prefix.len().saturating_add(edge_label.len()) <= 2048 {
                let mut next_prefix = String::with_capacity(prefix.len() + edge_label.len());
                next_prefix.push_str(&prefix);
                next_prefix.push_str(edge_label);
                stack.push((child_off, next_prefix));
            }
        }
    }
}

fn read_uleb128(bytes: &[u8], cursor: &mut usize) -> Option<u64> {
    let mut result = 0u64;
    let mut shift = 0u32;
    for _ in 0..10 {
        let byte = *bytes.get(*cursor)?;
        *cursor += 1;
        let low = u64::from(byte & 0x7f);
        if shift >= 64 || (shift == 63 && low > 1) {
            return None;
        }
        result |= low << shift;
        if (byte & 0x80) == 0 {
            return Some(result);
        }
        shift += 7;
    }
    None
}

fn read_cstr_slice<'a>(bytes: &'a [u8], cursor: &mut usize) -> Option<&'a str> {
    let rest = bytes.get(*cursor..)?;
    let nul = rest.iter().position(|&b| b == 0)?;
    *cursor += nul + 1;
    std::str::from_utf8(&rest[..nul]).ok()
}

const STRING_PAGE_SIZE: usize = 64 * 1024;
const STRING_PAGE_MASK: u64 = (STRING_PAGE_SIZE as u64) - 1;

struct PagedStringReader<'a> {
    source: &'a CacheSource,
    base_offset: u64,
    limit: u64,
    page_base: u64,
    page_len: usize,
    page: Vec<u8>,
}

impl<'a> PagedStringReader<'a> {
    fn new(source: &'a CacheSource, base_offset: u64, limit: u64) -> Self {
        Self {
            source,
            base_offset,
            limit,
            page_base: u64::MAX,
            page_len: 0,
            page: vec![0u8; STRING_PAGE_SIZE],
        }
    }

    fn read_cstr(&mut self, rel_offset: u64, max_len: usize) -> Option<String> {
        if rel_offset >= self.limit || max_len == 0 {
            return None;
        }
        let slice = self.slice_from(rel_offset)?;
        let cap = slice.len().min(max_len);
        if let Some(nul) = slice[..cap].iter().position(|&b| b == 0) {
            if nul == 0 {
                return None;
            }
            return std::str::from_utf8(&slice[..nul]).ok().map(str::to_owned);
        }
        if cap == max_len {
            return std::str::from_utf8(&slice[..max_len])
                .ok()
                .map(str::to_owned);
        }
        let mut out = Vec::with_capacity(cap.saturating_mul(2).min(max_len));
        out.extend_from_slice(&slice[..cap]);
        let mut cur = rel_offset.saturating_add(cap as u64);
        while out.len() < max_len && cur < self.limit {
            let next = self.slice_from(cur)?;
            let rem = (max_len - out.len()).min(next.len());
            if let Some(nul) = next[..rem].iter().position(|&b| b == 0) {
                out.extend_from_slice(&next[..nul]);
                break;
            }
            out.extend_from_slice(&next[..rem]);
            cur = cur.saturating_add(rem as u64);
        }
        if out.is_empty() {
            return None;
        }
        String::from_utf8(out).ok()
    }

    fn slice_from(&mut self, rel_offset: u64) -> Option<&[u8]> {
        let aligned = rel_offset & !STRING_PAGE_MASK;
        if aligned != self.page_base {
            let abs = self.base_offset.checked_add(aligned)?;
            let rem = self
                .limit
                .checked_sub(aligned)?
                .min(self.source.len().checked_sub(abs)?)
                .min(STRING_PAGE_SIZE as u64) as usize;
            if rem == 0 || !self.source.read_exact_at(abs, &mut self.page[..rem]) {
                self.page_base = u64::MAX;
                self.page_len = 0;
                return None;
            }
            self.page_base = aligned;
            self.page_len = rem;
        }
        let idx = (rel_offset - aligned) as usize;
        (idx < self.page_len).then(|| &self.page[idx..self.page_len])
    }
}

fn append_suffix(base: &Path, suffix: &str) -> PathBuf {
    let mut os = base.as_os_str().to_os_string();
    os.push(suffix);
    PathBuf::from(os)
}

#[cfg(test)]
mod tests {
    use super::{DyldSharedCache, LC_DYLD_EXPORTS_TRIE, MIN_HEADER_SUBCACHES_V2};
    use crate::macho::{LC_SEGMENT_64, MH_DYLIB_IN_CACHE, MachCpu};

    #[test]
    fn synthetic_multi_subcache_resolves_exports_local_symbols_and_unwind_info() {
        // Layout:
        // - Main cache (file 0): maps __TEXT [0x1800_0000_0 .. 0x1800_0400_0] at file offset 0x1000
        // - Subcache ".01" (file 1): maps __LINKEDIT [0x1c00_0000_0 .. 0x1c00_0200_0] at file offset 0x1000
        // - ".symbols": holds unredacted private local symbol `_private_helper`
        let text_vmaddr = 0x1_8000_0000u64;
        let text_vmsize = 0x4000u64;
        let linkedit_vmaddr = 0x1_c000_0000u64;
        let linkedit_vmsize = 0x2000u64;
        let sub_uuid = [0x11u8; 16];
        let sym_uuid = [0x22u8; 16];

        // Build subcache ".01" containing __LINKEDIT:
        // - nlist_64 table with one "<redacted>" entry + one "_public_symtab" entry
        // - export trie with "_exported_fn" at +0x240
        let mut sub_bytes = vec![0u8; 0x3000];
        write_cache_header(
            &mut sub_bytes,
            b"dyld_v1  arm64e\0",
            MIN_HEADER_SUBCACHES_V2,
            1,
            sub_uuid,
            text_vmaddr,
        );
        write_mapping(
            &mut sub_bytes,
            MIN_HEADER_SUBCACHES_V2 as usize,
            linkedit_vmaddr,
            linkedit_vmsize,
            0x1000,
        );
        let le_symoff = 0x1100u32;
        let le_stroff = 0x1200u32;
        let le_expoff = 0x1300u32;
        let mut le_str = vec![0u8];
        let red_strx = le_str.len() as u32;
        le_str.extend_from_slice(b"<redacted>\0");
        let pub_strx = le_str.len() as u32;
        le_str.extend_from_slice(b"_public_symtab\0");
        sub_bytes[le_stroff as usize..le_stroff as usize + le_str.len()].copy_from_slice(&le_str);
        write_nlist(
            &mut sub_bytes[le_symoff as usize..le_symoff as usize + 16],
            red_strx,
            text_vmaddr + 0x100,
        );
        write_nlist(
            &mut sub_bytes[le_symoff as usize + 16..le_symoff as usize + 32],
            pub_strx,
            text_vmaddr + 0x180,
        );
        // Minimal single-edge export trie: root -> "_exported_fn" -> terminal regular +0x240
        let trie = build_single_export_trie("_exported_fn", 0x240);
        sub_bytes[le_expoff as usize..le_expoff as usize + trie.len()].copy_from_slice(&trie);

        // Build ".symbols" file with local symbol `_private_helper` at +0x100.
        let mut sym_file = vec![0u8; 0x2000];
        write_cache_header(
            &mut sym_file,
            b"dyld_v1  arm64e\0",
            MIN_HEADER_SUBCACHES_V2,
            0,
            sym_uuid,
            text_vmaddr,
        );
        let local_off = 0x800usize;
        let local_size = 0x400u64;
        sym_file[0x48..0x50].copy_from_slice(&(local_off as u64).to_le_bytes());
        sym_file[0x50..0x58].copy_from_slice(&local_size.to_le_bytes());
        let nlist_rel = 24u32;
        let strings_rel = 64u32;
        let entries_rel = 128u32;
        sym_file[local_off..local_off + 4].copy_from_slice(&nlist_rel.to_le_bytes());
        sym_file[local_off + 4..local_off + 8].copy_from_slice(&1u32.to_le_bytes());
        sym_file[local_off + 8..local_off + 12].copy_from_slice(&strings_rel.to_le_bytes());
        sym_file[local_off + 12..local_off + 16].copy_from_slice(&32u32.to_le_bytes());
        sym_file[local_off + 16..local_off + 20].copy_from_slice(&entries_rel.to_le_bytes());
        sym_file[local_off + 20..local_off + 24].copy_from_slice(&1u32.to_le_bytes());
        let local_str = b"\0_private_helper\0";
        sym_file
            [local_off + strings_rel as usize..local_off + strings_rel as usize + local_str.len()]
            .copy_from_slice(local_str);
        write_nlist(
            &mut sym_file[local_off + nlist_rel as usize..local_off + nlist_rel as usize + 16],
            1,
            text_vmaddr + 0x100,
        );
        // 16-byte entry_64: dylib_offset = 0 (header_vmaddr - shared_region_start), start=0, count=1
        sym_file[local_off + entries_rel as usize..local_off + entries_rel as usize + 8]
            .copy_from_slice(&0u64.to_le_bytes());
        sym_file[local_off + entries_rel as usize + 8..local_off + entries_rel as usize + 12]
            .copy_from_slice(&0u32.to_le_bytes());
        sym_file[local_off + entries_rel as usize + 12..local_off + entries_rel as usize + 16]
            .copy_from_slice(&1u32.to_le_bytes());

        // Build main cache (file 0) with dylib at 0x1800_0000_0 (file offset 0x1000).
        let mut main_bytes = vec![0u8; 0x5000];
        write_cache_header(
            &mut main_bytes,
            b"dyld_v1  arm64e\0",
            MIN_HEADER_SUBCACHES_V2,
            1,
            [0x33u8; 16],
            text_vmaddr,
        );
        write_mapping(
            &mut main_bytes,
            MIN_HEADER_SUBCACHES_V2 as usize,
            text_vmaddr,
            text_vmsize,
            0x1000,
        );
        // Subcache V2 table at 0x400, image table at 0x500, path string at 0x600.
        main_bytes[0x188..0x18c].copy_from_slice(&0x400u32.to_le_bytes());
        main_bytes[0x18c..0x190].copy_from_slice(&1u32.to_le_bytes());
        main_bytes[0x190..0x1a0].copy_from_slice(&sym_uuid);
        main_bytes[0x1c0..0x1c4].copy_from_slice(&0x500u32.to_le_bytes());
        main_bytes[0x1c4..0x1c8].copy_from_slice(&1u32.to_le_bytes());
        main_bytes[0x400..0x410].copy_from_slice(&sub_uuid);
        main_bytes[0x418..0x41b].copy_from_slice(b".01");
        main_bytes[0x500..0x508].copy_from_slice(&text_vmaddr.to_le_bytes());
        main_bytes[0x518..0x51c].copy_from_slice(&0x600u32.to_le_bytes());
        let dylib_path = "/usr/lib/system/libsystem_kernel.dylib";
        main_bytes[0x600..0x600 + dylib_path.len()].copy_from_slice(dylib_path.as_bytes());

        // Write cached Mach-O header + load commands at 0x1000:
        // __TEXT (with __text and __unwind_info), __LINKEDIT, LC_SYMTAB, LC_DYLD_EXPORTS_TRIE
        let dylib_macho = build_cached_dylib(
            text_vmaddr,
            text_vmsize,
            linkedit_vmaddr,
            linkedit_vmsize,
            (le_symoff, 2, le_stroff, le_str.len() as u32),
            (le_expoff, trie.len() as u32),
        );
        main_bytes[0x1000..0x1000 + dylib_macho.len()].copy_from_slice(&dylib_macho);
        // Fake __unwind_info payload at +0x800 inside __TEXT (file offset 0x1800)
        main_bytes[0x1800..0x181c].copy_from_slice(&[
            1, 0, 0, 0, 28, 0, 0, 0, 0, 0, 0, 0, 28, 0, 0, 0, 0, 0, 0, 0, 28, 0, 0, 0, 0, 0, 0, 0,
        ]);

        let cache = DyldSharedCache::from_memory_parts(
            main_bytes,
            vec![(".01".to_string(), sub_bytes)],
            Some(sym_file),
            MachCpu::Arm64,
        )
        .expect("parse synthetic dyld shared cache");

        let slide = 0x0020_0000u64;
        let runtime_load = text_vmaddr + slide;
        let image = cache
            .load_image(dylib_path, runtime_load, "libsystem_kernel.dylib")
            .expect("load cached dylib");
        assert_eq!(image.start, runtime_load);
        assert_eq!(image.end, runtime_load + text_vmsize);
        assert_eq!(
            image.name_at(runtime_load + 0x100).map(|(n, _)| n),
            Some("private_helper"),
            "<redacted> should be ignored and replaced by .symbols local symbol"
        );
        assert_eq!(
            image.name_at(runtime_load + 0x180).map(|(n, _)| n),
            Some("public_symtab")
        );
        assert_eq!(
            image.name_at(runtime_load + 0x240).map(|(n, _)| n),
            Some("exported_fn")
        );
        let mut unwinder = framehop::UnwinderNative::<
            crate::walk::SectionBytes,
            framehop::MayAllocateDuringUnwind,
        >::new();
        image.register_unwind_module(&mut unwinder);
        use framehop::Unwinder as _;
        assert_eq!(
            unwinder.max_known_code_address(),
            runtime_load + text_vmsize
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn host_dyld_shared_cache_loads_libsystem_kernel_symbols_and_unwind() {
        use framehop::Unwinder as _;
        let cpu = MachCpu::host();
        let cache = DyldSharedCache::open_host(cpu).expect("open host dyld_shared_cache");
        let load_addr = 0x1_9000_0000u64;
        let image = cache
            .load_image(
                "/usr/lib/system/libsystem_kernel.dylib",
                load_addr,
                "libsystem_kernel.dylib",
            )
            .expect("load libsystem_kernel.dylib from host cache");
        assert!(image.end > image.start);
        assert!(
            image.address_of("read").is_some()
                || image.address_of("mach_msg2_trap").is_some()
                || image.address_of("mach_msg_trap").is_some(),
            "expected kernel syscall wrapper symbols in libsystem_kernel.dylib"
        );
        let mut unwinder = framehop::UnwinderNative::<
            crate::walk::SectionBytes,
            framehop::MayAllocateDuringUnwind,
        >::new();
        image.register_unwind_module(&mut unwinder);
        assert!(unwinder.max_known_code_address() > load_addr);
    }

    fn write_cache_header(
        buf: &mut [u8],
        magic: &[u8; 16],
        mapping_offset: u32,
        mapping_count: u32,
        uuid: [u8; 16],
        shared_region_start: u64,
    ) {
        buf[0..16].copy_from_slice(magic);
        buf[0x10..0x14].copy_from_slice(&mapping_offset.to_le_bytes());
        buf[0x14..0x18].copy_from_slice(&mapping_count.to_le_bytes());
        buf[0x58..0x68].copy_from_slice(&uuid);
        buf[0xe0..0xe8].copy_from_slice(&shared_region_start.to_le_bytes());
    }

    fn write_mapping(buf: &mut [u8], at: usize, vmaddr: u64, vmsize: u64, file_off: u64) {
        buf[at..at + 8].copy_from_slice(&vmaddr.to_le_bytes());
        buf[at + 8..at + 16].copy_from_slice(&vmsize.to_le_bytes());
        buf[at + 16..at + 24].copy_from_slice(&file_off.to_le_bytes());
    }

    fn write_nlist(entry: &mut [u8], strx: u32, value: u64) {
        entry[0..4].copy_from_slice(&strx.to_le_bytes());
        entry[4] = 0x0e; // N_SECT
        entry[5] = 1;
        entry[8..16].copy_from_slice(&value.to_le_bytes());
    }

    fn build_single_export_trie(sym: &str, sym_off: u64) -> Vec<u8> {
        // Node 0: terminal_size=0, child_count=1, label=sym\0, child_off
        // Child node: terminal_size=3, flags=0, uleb(sym_off, 2 bytes), child_count=0
        let mut out = Vec::new();
        out.push(0); // root terminal_size = 0
        out.push(1); // 1 child
        out.extend_from_slice(sym.as_bytes());
        out.push(0);
        let child_off = (out.len() + 1) as u8;
        out.push(child_off);
        // child node:
        let mut term = vec![0u8]; // flags = REGULAR (0)
        encode_uleb128(sym_off, &mut term);
        out.push(term.len() as u8);
        out.extend_from_slice(&term);
        out.push(0); // 0 children
        out
    }

    fn encode_uleb128(mut v: u64, out: &mut Vec<u8>) {
        loop {
            let mut byte = (v & 0x7f) as u8;
            v >>= 7;
            if v != 0 {
                byte |= 0x80;
            }
            out.push(byte);
            if v == 0 {
                break;
            }
        }
    }

    fn build_cached_dylib(
        text_vmaddr: u64,
        text_vmsize: u64,
        le_vmaddr: u64,
        le_vmsize: u64,
        symtab: (u32, u32, u32, u32),
        exports: (u32, u32),
    ) -> Vec<u8> {
        let mut cmds = Vec::new();
        // __TEXT segment with 2 sections (__text, __unwind_info)
        let text_cmd_len = 72 + 2 * 80;
        let mut text_seg = vec![0u8; text_cmd_len];
        text_seg[0..4].copy_from_slice(&LC_SEGMENT_64.to_le_bytes());
        text_seg[4..8].copy_from_slice(&(text_cmd_len as u32).to_le_bytes());
        text_seg[8..14].copy_from_slice(b"__TEXT");
        text_seg[24..32].copy_from_slice(&text_vmaddr.to_le_bytes());
        text_seg[32..40].copy_from_slice(&text_vmsize.to_le_bytes());
        text_seg[64..68].copy_from_slice(&2u32.to_le_bytes()); // nsects = 2
        // sec 0: __text
        let s0 = &mut text_seg[72..152];
        s0[0..6].copy_from_slice(b"__text");
        s0[16..22].copy_from_slice(b"__TEXT");
        s0[32..40].copy_from_slice(&(text_vmaddr + 0x100).to_le_bytes());
        s0[40..48].copy_from_slice(&0x400u64.to_le_bytes());
        // sec 1: __unwind_info at +0x800
        let s1 = &mut text_seg[152..232];
        s1[0..13].copy_from_slice(b"__unwind_info");
        s1[16..22].copy_from_slice(b"__TEXT");
        s1[32..40].copy_from_slice(&(text_vmaddr + 0x800).to_le_bytes());
        s1[40..48].copy_from_slice(&28u64.to_le_bytes());
        cmds.extend_from_slice(&text_seg);

        // __LINKEDIT segment
        let mut le_seg = vec![0u8; 72];
        le_seg[0..4].copy_from_slice(&LC_SEGMENT_64.to_le_bytes());
        le_seg[4..8].copy_from_slice(&72u32.to_le_bytes());
        le_seg[8..18].copy_from_slice(b"__LINKEDIT");
        le_seg[24..32].copy_from_slice(&le_vmaddr.to_le_bytes());
        le_seg[32..40].copy_from_slice(&le_vmsize.to_le_bytes());
        cmds.extend_from_slice(&le_seg);

        // LC_SYMTAB (0x2)
        let mut sym_cmd = vec![0u8; 24];
        sym_cmd[0..4].copy_from_slice(&2u32.to_le_bytes());
        sym_cmd[4..8].copy_from_slice(&24u32.to_le_bytes());
        sym_cmd[8..12].copy_from_slice(&symtab.0.to_le_bytes());
        sym_cmd[12..16].copy_from_slice(&symtab.1.to_le_bytes());
        sym_cmd[16..20].copy_from_slice(&symtab.2.to_le_bytes());
        sym_cmd[20..24].copy_from_slice(&symtab.3.to_le_bytes());
        cmds.extend_from_slice(&sym_cmd);

        // LC_DYLD_EXPORTS_TRIE
        let mut exp_cmd = vec![0u8; 16];
        exp_cmd[0..4].copy_from_slice(&LC_DYLD_EXPORTS_TRIE.to_le_bytes());
        exp_cmd[4..8].copy_from_slice(&16u32.to_le_bytes());
        exp_cmd[8..12].copy_from_slice(&exports.0.to_le_bytes());
        exp_cmd[12..16].copy_from_slice(&exports.1.to_le_bytes());
        cmds.extend_from_slice(&exp_cmd);

        let mut hdr = vec![0u8; 32];
        hdr[0..4].copy_from_slice(&0xFEED_FACFu32.to_le_bytes());
        hdr[4..8].copy_from_slice(&0x0100_000Cu32.to_le_bytes());
        hdr[12..16].copy_from_slice(&6u32.to_le_bytes()); // MH_DYLIB
        hdr[16..20].copy_from_slice(&4u32.to_le_bytes()); // ncmds = 4
        hdr[20..24].copy_from_slice(&(cmds.len() as u32).to_le_bytes());
        hdr[24..28].copy_from_slice(&MH_DYLIB_IN_CACHE.to_le_bytes());
        hdr.extend_from_slice(&cmds);
        hdr
    }
}
