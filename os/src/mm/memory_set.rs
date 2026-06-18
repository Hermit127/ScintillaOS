use super::vpn_range::VAddrRange;
use super::{frame_alloc, FrameTracker};
use crate::config::{PAGE_SIZE, USER_STACK_SIZE};
use crate::fs::File;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use log::*;
use polyhal::pagetable::{MappingFlags, MappingSize, PageTable, PageTableWrapper};
use polyhal::{PhysAddr, VirtAddr};
use xmas_elf::header::Type as ElfType;
pub struct MemorySet {
    page_table: Arc<PageTableWrapper>,
    areas: Vec<MapArea>,
}

pub struct LoadedElf {
    pub memory_set: MemorySet,
    pub user_sp: usize,
    pub entry_point: usize,
    pub program_break: usize,
    pub ph_addr: usize,
    pub ph_entsize: usize,
    pub ph_num: usize,
    pub tls_base: usize,
    pub tls_size: usize,
    pub interp_path: Option<alloc::string::String>,
    pub interp_base: usize,
    pub program_entry: usize,
}

pub fn get_elf_interp(elf_data: &[u8]) -> Option<String> {
    let elf = match xmas_elf::ElfFile::new(elf_data) {
        Ok(e) => e,
        Err(_) => return None,
    };
    let ph_count = elf.header.pt2.ph_count();
    for i in 0..ph_count {
        let ph = match elf.program_header(i) {
            Ok(p) => p,
            Err(_) => continue,
        };
        if ph.get_type().unwrap() == xmas_elf::program::Type::Interp {
            let offset = ph.offset() as usize;
            let size = ph.file_size() as usize;
            if offset + size <= elf_data.len() {
                let raw = &elf_data[offset..offset + size];
                let end = raw.iter().position(|&b| b == 0).unwrap_or(size);
                return core::str::from_utf8(&raw[..end]).ok().map(|s| s.into());
            }
        }
    }
    None
}

const ELF_HEADER_READ_SIZE: usize = 64;
const MAX_ELF_HEADER_TABLE_SIZE: usize = 64 * 1024;
const MAX_INTERP_PATH_LEN: usize = 4096;

fn read_exact_at(file: &dyn File, mut offset: usize, buf: &mut [u8]) -> bool {
    let mut done = 0usize;
    while done < buf.len() {
        let n = file.read_at(offset, &mut buf[done..]);
        if n <= 0 {
            return false;
        }
        let n = n as usize;
        done += n;
        offset += n;
    }
    true
}

fn read_elf_header_table(file: &dyn File) -> Option<Vec<u8>> {
    let mut ident = [0u8; ELF_HEADER_READ_SIZE];
    if !read_exact_at(file, 0, &mut ident) {
        return None;
    }
    if ident[..4] != [0x7f, b'E', b'L', b'F'] {
        return None;
    }
    let header = xmas_elf::ElfFile::new(&ident).ok()?.header;
    let ph_offset = header.pt2.ph_offset() as usize;
    let ph_entsize = header.pt2.ph_entry_size() as usize;
    let ph_count = header.pt2.ph_count() as usize;
    if ph_offset == 0 || ph_entsize == 0 || ph_count == 0 {
        return None;
    }
    let ph_size = ph_entsize.checked_mul(ph_count)?;
    let total = ph_offset.checked_add(ph_size)?;
    if total < ELF_HEADER_READ_SIZE || total > MAX_ELF_HEADER_TABLE_SIZE {
        return None;
    }
    let mut data = alloc::vec![0u8; total];
    if !read_exact_at(file, 0, &mut data) {
        return None;
    }
    Some(data)
}

fn read_small_file_region(file: &dyn File, offset: usize, size: usize) -> Option<Vec<u8>> {
    if size > MAX_INTERP_PATH_LEN {
        return None;
    }
    let mut data = alloc::vec![0u8; size];
    if !read_exact_at(file, offset, &mut data) {
        return None;
    }
    Some(data)
}

pub fn get_elf_interp_file(file: &dyn File) -> Option<String> {
    let header_data = read_elf_header_table(file)?;
    let elf = xmas_elf::ElfFile::new(&header_data).ok()?;
    let ph_count = elf.header.pt2.ph_count();
    for i in 0..ph_count {
        let ph = elf.program_header(i).ok()?;
        if ph.get_type().ok()? == xmas_elf::program::Type::Interp {
            let offset = ph.offset() as usize;
            let size = ph.file_size() as usize;
            let raw = read_small_file_region(file, offset, size)?;
            let end = raw.iter().position(|&b| b == 0).unwrap_or(size);
            return core::str::from_utf8(&raw[..end]).ok().map(|s| s.into());
        }
    }
    None
}

impl MemorySet {
    pub fn new_bare() -> Self {
        Self {
            page_table: Arc::new(PageTableWrapper::alloc()),
            areas: Vec::new(),
        }
    }
    pub fn token(&self) -> PageTable {
        self.page_table.0
    }
    pub fn push(&mut self, mut map_area: MapArea, data: Option<&[u8]>, page_off: usize) -> bool {
        if map_area.map(&self.page_table).is_err() {
            return false;
        }
        if let Some(data) = data {
            map_area.copy_data(&self.page_table, data, page_off);
        }
        self.areas.push(map_area);
        true
    }

    fn push_file_segment(
        &mut self,
        mut map_area: MapArea,
        file: &dyn File,
        file_offset: usize,
        file_size: usize,
        page_off: usize,
    ) -> bool {
        if map_area.map(&self.page_table).is_err() {
            return false;
        }
        if !map_area.copy_file_data(file, file_offset, file_size, page_off) {
            map_area.unmap(&self.page_table);
            return false;
        }
        self.areas.push(map_area);
        true
    }
    pub fn from_elf(elf_data: &[u8]) -> LoadedElf {
        trace!("os::mm::MemorySet::from_elf");
        let mut memory_set = Self::new_bare();
        let elf = xmas_elf::ElfFile::new(elf_data).unwrap();
        let elf_header = elf.header;
        let magic = elf_header.pt1.magic;
        assert_eq!(magic, [0x7f, 0x45, 0x4c, 0x46], "invalid elf!");
        let ph_count = elf_header.pt2.ph_count();
        let e_phoff = elf_header.pt2.ph_offset() as usize;
        let e_phentsize = elf_header.pt2.ph_entry_size() as usize;
        let is_pie = elf.header.pt2.type_().as_type() == ElfType::SharedObject;
        let pie_base: usize = if is_pie { 0x1_0000 } else { 0 };
        let mut max_end_va = VirtAddr::new(0);
        let mut ph_addr = 0usize;
        let mut first_load = true;
        let mut tls_base: usize = 0;
        let mut tls_size: usize = 0;
        let mut interp_path: Option<alloc::string::String> = None;
        let program_entry = elf.header.pt2.entry_point() as usize + pie_base;
        for i in 0..ph_count {
            let ph = elf.program_header(i).unwrap();
            let va_offset = pie_base;
            if ph.get_type().unwrap() == xmas_elf::program::Type::Load {
                let start_va: VirtAddr = (ph.virtual_addr() as usize + va_offset).into();
                let end_va: VirtAddr =
                    ((ph.virtual_addr() + ph.mem_size()) as usize + va_offset).into();
                if first_load {
                    ph_addr = start_va.raw() + e_phoff;
                    first_load = false;
                }
                let mut map_perm = MapPermission::U;
                let ph_flags = ph.flags();
                if ph_flags.is_read() {
                    map_perm |= MapPermission::R;
                }
                if ph_flags.is_write() {
                    map_perm |= MapPermission::W;
                }
                if ph_flags.is_execute() {
                    map_perm |= MapPermission::X;
                }
                let map_area = MapArea::new(start_va, end_va, MapType::Framed, map_perm);
                max_end_va = max_end_va.max(map_area.vaddr_range.get_end());
                let page_off = start_va.raw() % PAGE_SIZE;
                if page_off > 0 {
                    let aligned_va = start_va.raw() - page_off;
                    let file_start = ph.offset() as usize - page_off;
                    let file_end = (ph.offset() + ph.file_size()) as usize;
                    let aligned_data = &elf.input[file_start..file_end];
                    let aligned_map =
                        MapArea::new(aligned_va.into(), end_va, MapType::Framed, map_perm);
                    memory_set.push(aligned_map, Some(aligned_data), 0);
                } else {
                    memory_set.push(
                        map_area,
                        Some(
                            &elf.input
                                [ph.offset() as usize..(ph.offset() + ph.file_size()) as usize],
                        ),
                        0,
                    );
                }
            } else if ph.get_type().unwrap() == xmas_elf::program::Type::Tls {
                tls_base = ph.virtual_addr() as usize + va_offset;
                tls_size = ph.mem_size() as usize;
            } else if ph.get_type().unwrap() == xmas_elf::program::Type::Interp {
                let offset = ph.offset() as usize;
                let size = ph.file_size() as usize;
                if offset + size <= elf_data.len() {
                    let raw = &elf_data[offset..offset + size];
                    let end = raw.iter().position(|&b| b == 0).unwrap_or(size);
                    interp_path = core::str::from_utf8(&raw[..end]).ok().map(|s| s.into());
                }
            }
        }
        let program_break = (max_end_va.raw() + PAGE_SIZE - 1) / PAGE_SIZE * PAGE_SIZE;
        let user_stack_top: usize = 0x8_0000_0000;
        let user_stack_bottom = user_stack_top - USER_STACK_SIZE;
        let user_stack_top = user_stack_bottom + USER_STACK_SIZE;
        memory_set.push(
            MapArea::new(
                user_stack_bottom.into(),
                user_stack_top.into(),
                MapType::Framed,
                MapPermission::R | MapPermission::W | MapPermission::U,
            ),
            None,
            0,
        );
        LoadedElf {
            memory_set,
            user_sp: user_stack_top,
            entry_point: program_entry,
            program_break,
            ph_addr,
            ph_entsize: e_phentsize,
            ph_num: ph_count as usize,
            tls_base,
            tls_size,
            interp_path,
            interp_base: 0,
            program_entry,
        }
    }

    pub fn from_elf_file(file: &dyn File) -> Option<LoadedElf> {
        trace!("os::mm::MemorySet::from_elf_file");
        let header_data = read_elf_header_table(file)?;
        let elf = xmas_elf::ElfFile::new(&header_data).ok()?;
        let elf_header = elf.header;
        if elf_header.pt1.magic != [0x7f, 0x45, 0x4c, 0x46] {
            return None;
        }
        let ph_count = elf_header.pt2.ph_count();
        let e_phoff = elf_header.pt2.ph_offset() as usize;
        let e_phentsize = elf_header.pt2.ph_entry_size() as usize;
        let is_pie = elf.header.pt2.type_().as_type() == ElfType::SharedObject;
        let pie_base: usize = if is_pie { 0x1_0000 } else { 0 };
        let mut memory_set = Self::new_bare();
        let mut max_end_va = VirtAddr::new(0);
        let mut ph_addr = 0usize;
        let mut first_load = true;
        let mut tls_base: usize = 0;
        let mut tls_size: usize = 0;
        let mut interp_path: Option<alloc::string::String> = None;
        let program_entry = elf.header.pt2.entry_point() as usize + pie_base;

        for i in 0..ph_count {
            let ph = elf.program_header(i).ok()?;
            let ph_type = ph.get_type().ok()?;
            let va_offset = pie_base;
            if ph_type == xmas_elf::program::Type::Load {
                let start_va: VirtAddr = (ph.virtual_addr() as usize + va_offset).into();
                let end_va: VirtAddr =
                    ((ph.virtual_addr() + ph.mem_size()) as usize + va_offset).into();
                if first_load {
                    ph_addr = start_va.raw() + e_phoff;
                    first_load = false;
                }
                let mut map_perm = MapPermission::U;
                let ph_flags = ph.flags();
                if ph_flags.is_read() {
                    map_perm |= MapPermission::R;
                }
                if ph_flags.is_write() {
                    map_perm |= MapPermission::W;
                }
                if ph_flags.is_execute() {
                    map_perm |= MapPermission::X;
                }
                let page_off = start_va.raw() % PAGE_SIZE;
                let aligned_start = start_va.raw() - page_off;
                let map_area =
                    MapArea::new(aligned_start.into(), end_va, MapType::Framed, map_perm);
                max_end_va = max_end_va.max(map_area.vaddr_range.get_end());
                if !memory_set.push_file_segment(
                    map_area,
                    file,
                    ph.offset() as usize,
                    ph.file_size() as usize,
                    page_off,
                ) {
                    return None;
                }
            } else if ph_type == xmas_elf::program::Type::Tls {
                tls_base = ph.virtual_addr() as usize + va_offset;
                tls_size = ph.mem_size() as usize;
            } else if ph_type == xmas_elf::program::Type::Interp {
                let offset = ph.offset() as usize;
                let size = ph.file_size() as usize;
                let raw = read_small_file_region(file, offset, size)?;
                let end = raw.iter().position(|&b| b == 0).unwrap_or(size);
                interp_path = core::str::from_utf8(&raw[..end]).ok().map(|s| s.into());
            }
        }

        let program_break = (max_end_va.raw() + PAGE_SIZE - 1) / PAGE_SIZE * PAGE_SIZE;
        let user_stack_top: usize = 0x8_0000_0000;
        let user_stack_bottom = user_stack_top - USER_STACK_SIZE;
        let user_stack_top = user_stack_bottom + USER_STACK_SIZE;
        if !memory_set.push(
            MapArea::new(
                user_stack_bottom.into(),
                user_stack_top.into(),
                MapType::Framed,
                MapPermission::R | MapPermission::W | MapPermission::U,
            ),
            None,
            0,
        ) {
            return None;
        }
        Some(LoadedElf {
            memory_set,
            user_sp: user_stack_top,
            entry_point: program_entry,
            program_break,
            ph_addr,
            ph_entsize: e_phentsize,
            ph_num: ph_count as usize,
            tls_base,
            tls_size,
            interp_path,
            interp_base: 0,
            program_entry,
        })
    }

    /// Load a dynamic linker/interpreter ELF at a fixed base address.
    /// Returns (entry_point, base_addr).
    pub fn load_interp_elf(
        memory_set: &mut MemorySet,
        interp_data: &[u8],
        base: usize,
    ) -> (usize, usize) {
        let elf = xmas_elf::ElfFile::new(interp_data).unwrap();
        let entry = elf.header.pt2.entry_point() as usize + base;
        let ph_count = elf.header.pt2.ph_count();
        for i in 0..ph_count {
            let ph = elf.program_header(i).unwrap();
            if ph.get_type().unwrap() == xmas_elf::program::Type::Load {
                let start_va: VirtAddr = (ph.virtual_addr() as usize + base).into();
                let end_va: VirtAddr = ((ph.virtual_addr() + ph.mem_size()) as usize + base).into();
                let mut map_perm = MapPermission::U;
                let ph_flags = ph.flags();
                if ph_flags.is_read() {
                    map_perm |= MapPermission::R;
                }
                if ph_flags.is_write() {
                    map_perm |= MapPermission::W;
                }
                if ph_flags.is_execute() {
                    map_perm |= MapPermission::X;
                }
                let map_area = MapArea::new(start_va, end_va, MapType::Framed, map_perm);
                let page_off = start_va.raw() % PAGE_SIZE;
                if page_off > 0 {
                    let aligned_va = start_va.raw() - page_off;
                    let file_start = ph.offset() as usize - page_off;
                    let file_end = (ph.offset() + ph.file_size()) as usize;
                    let aligned_data = &interp_data[file_start..file_end];
                    let aligned_map =
                        MapArea::new(aligned_va.into(), end_va, MapType::Framed, map_perm);
                    memory_set.push(aligned_map, Some(aligned_data), 0);
                } else {
                    memory_set.push(
                        map_area,
                        Some(
                            &interp_data
                                [ph.offset() as usize..(ph.offset() + ph.file_size()) as usize],
                        ),
                        0,
                    );
                }
            }
        }
        (entry, base)
    }

    pub fn load_interp_file(
        memory_set: &mut MemorySet,
        file: &dyn File,
        base: usize,
    ) -> Option<(usize, usize)> {
        let header_data = read_elf_header_table(file)?;
        let elf = xmas_elf::ElfFile::new(&header_data).ok()?;
        let entry = elf.header.pt2.entry_point() as usize + base;
        let ph_count = elf.header.pt2.ph_count();
        for i in 0..ph_count {
            let ph = elf.program_header(i).ok()?;
            if ph.get_type().ok()? == xmas_elf::program::Type::Load {
                let start_va: VirtAddr = (ph.virtual_addr() as usize + base).into();
                let end_va: VirtAddr = ((ph.virtual_addr() + ph.mem_size()) as usize + base).into();
                let mut map_perm = MapPermission::U;
                let ph_flags = ph.flags();
                if ph_flags.is_read() {
                    map_perm |= MapPermission::R;
                }
                if ph_flags.is_write() {
                    map_perm |= MapPermission::W;
                }
                if ph_flags.is_execute() {
                    map_perm |= MapPermission::X;
                }
                let page_off = start_va.raw() % PAGE_SIZE;
                let aligned_start = start_va.raw() - page_off;
                let map_area =
                    MapArea::new(aligned_start.into(), end_va, MapType::Framed, map_perm);
                if !memory_set.push_file_segment(
                    map_area,
                    file,
                    ph.offset() as usize,
                    ph.file_size() as usize,
                    page_off,
                ) {
                    return None;
                }
            }
        }
        Some((entry, base))
    }

    pub fn from_existed_user(user_space: &MemorySet) -> Option<MemorySet> {
        trace!("os::mm::MemorySet::from_existed_user");
        let mut memory_set = Self::new_bare();
        for area in user_space.areas.iter() {
            let mut new_area = MapArea::from_another(area);
            for (&vpn, frame) in area.data_frames.iter() {
                let new_frame = frame.shared_clone();
                let mut child_perm = area.map_perm;
                if !area.shared && child_perm.contains(MapPermission::W) {
                    child_perm.remove(MapPermission::W);
                    user_space.page_table.unmap_page(vpn);
                    user_space.page_table.map_page(
                        vpn,
                        frame.paddr,
                        child_perm.into(),
                        MappingSize::Page4KB,
                    );
                }
                memory_set.page_table.map_page(
                    vpn,
                    new_frame.paddr,
                    child_perm.into(),
                    MappingSize::Page4KB,
                );
                new_area.data_frames.insert(vpn, new_frame);
            }
            memory_set.areas.push(new_area);
        }
        Some(memory_set)
    }

    pub fn handle_page_fault(&mut self, vaddr: polyhal::VirtAddr, write_fault: bool) -> bool {
        let vpn = vaddr.floor();
        let area = match self
            .areas
            .iter_mut()
            .find(|a| a.vaddr_range.get_start() <= vpn && vpn < a.vaddr_range.get_end())
        {
            Some(a) => a,
            None => return false,
        };
        let in_frames = area.data_frames.contains_key(&vpn);
        if !in_frames {
            let f = match frame_alloc() {
                Some(f) => f,
                None => return false,
            };
            if let Some(ref file) = area.file {
                let off = area.file_offset + (vpn.raw() - area.vaddr_range.get_start().raw());
                let buf = f.paddr.slice_mut_with_len(crate::config::PAGE_SIZE);
                let _ = file.read_at(off, buf);
            }
            self.page_table
                .map_page(vpn, f.paddr, area.map_perm.into(), MappingSize::Page4KB);
            area.data_frames.insert(vpn, f);
            return true;
        }
        // Write fault on read-only area → SIGSEGV
        if write_fault && !area.map_perm.contains(MapPermission::W) {
            return false;
        }
        // Read fault on already-mapped read-only page — re-establish PTE
        if !write_fault && !area.map_perm.contains(MapPermission::W) {
            let paddr = match area.data_frames.get(&vpn) {
                Some(f) => f.paddr,
                None => return false,
            };
            self.page_table.unmap_page(vpn);
            self.page_table
                .map_page(vpn, paddr, area.map_perm.into(), MappingSize::Page4KB);
            return true;
        }
        // COW or permission upgrade for writable pages
        let (paddr, _) = match self.page_table.translate(vpn) {
            Some(x) => x,
            None => return false,
        };
        let old = match area.data_frames.get(&vpn) {
            Some(f) => f,
            None => return false,
        };
        if old.is_shared() {
            let f = match frame_alloc() {
                Some(f) => f,
                None => return false,
            };
            unsafe {
                core::ptr::copy_nonoverlapping(
                    paddr.get_ptr::<u8>(),
                    f.paddr.get_mut_ptr(),
                    PAGE_SIZE,
                );
            }
            self.page_table.unmap_page(vpn);
            self.page_table
                .map_page(vpn, f.paddr, area.map_perm.into(), MappingSize::Page4KB);
            area.data_frames.insert(vpn, f);
        } else {
            self.page_table.unmap_page(vpn);
            self.page_table
                .map_page(vpn, old.paddr, area.map_perm.into(), MappingSize::Page4KB);
        }
        true
    }
    pub fn from_shared(user_space: &MemorySet) -> MemorySet {
        MemorySet {
            page_table: user_space.page_table.clone(),
            areas: Vec::new(),
        }
    }
    pub fn activate(&self) {
        self.page_table.change();
    }
    pub fn translate(&self, vaddr: VirtAddr) -> Option<(PhysAddr, MappingFlags)> {
        self.page_table
            .translate(vaddr)
            .map(|(pa, flags)| (pa.into(), flags))
    }
    pub fn valid_user_buf(&self, ptr: *const u8, len: usize, need_write: bool) -> bool {
        if len == 0 {
            return true;
        }
        let addr = ptr as usize;
        let addr_end = addr.checked_add(len.saturating_sub(1));
        if addr_end.is_none() {
            return false;
        }
        let addr_end = addr_end.unwrap();
        if addr_end < addr {
            return false;
        }
        let start = addr & !(crate::config::PAGE_SIZE - 1);
        let end = (addr_end & !(crate::config::PAGE_SIZE - 1)) + crate::config::PAGE_SIZE;
        if end < start {
            return false;
        }
        let mut vpn = start;
        while vpn < end {
            let vaddr = VirtAddr::from(vpn);
            let Some(area) = self.areas.iter().find(|area| {
                area.vaddr_range.get_start() <= vaddr && vaddr < area.vaddr_range.get_end()
            }) else {
                return false;
            };
            if !area.map_perm.contains(MapPermission::U) {
                return false;
            }
            if need_write {
                if !area.map_perm.contains(MapPermission::W) {
                    return false;
                }
            } else if !area.map_perm.contains(MapPermission::R) {
                return false;
            }
            if self.translate(vaddr).is_none() {
                return false;
            }
            vpn += crate::config::PAGE_SIZE;
        }
        true
    }
    pub fn recycle_data_pages(&mut self) {
        for area in self.areas.iter_mut() {
            area.unmap(&self.page_table);
        }
        self.areas.clear();
    }
    pub fn shrink_to(&mut self, limit_va: VirtAddr) {
        self.areas.retain_mut(|area| {
            if area.vaddr_range.get_start() >= limit_va {
                area.unmap(&self.page_table);
                false
            } else {
                true
            }
        });
    }
    pub fn is_overlap(&self, start_addr: polyhal::VirtAddr, end_addr: polyhal::VirtAddr) -> bool {
        for area in self.areas.iter() {
            if start_addr < area.vaddr_range.get_end() && end_addr > area.vaddr_range.get_start() {
                return true;
            }
        }
        false
    }

    pub fn find_free_area(&self, len: usize) -> polyhal::VirtAddr {
        let len = (len + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
        if self.areas.is_empty() {
            return polyhal::VirtAddr::from(PAGE_SIZE);
        }
        let mut bounds: alloc::vec::Vec<(usize, usize)> = self
            .areas
            .iter()
            .map(|a| {
                let s: usize = a.vaddr_range.get_start().into();
                let e: usize = a.vaddr_range.get_end().into();
                (s, e)
            })
            .collect();
        bounds.sort_by_key(|(s, _)| *s);
        let mut cursor: usize = PAGE_SIZE;
        for (start, end) in bounds {
            if cursor + len <= start {
                return polyhal::VirtAddr::from(cursor);
            }
            if end > cursor {
                cursor = end;
            }
        }
        polyhal::VirtAddr::from(cursor)
    }

    pub fn page_table_mut(&mut self) -> &Arc<PageTableWrapper> {
        &self.page_table
    }
    pub fn areas_mut(&mut self) -> &mut Vec<MapArea> {
        &mut self.areas
    }

    /// 插入匿名映射
    pub fn insert_framed_area(
        &mut self,
        start_addr: polyhal::VirtAddr,
        end_addr: polyhal::VirtAddr,
        permission: MapPermission,
    ) -> bool {
        let mut map_area = MapArea::new(start_addr, end_addr, MapType::Framed, permission);
        if map_area.map(&self.page_table).is_err() {
            return false;
        }
        self.areas.push(map_area);
        true
    }

    pub fn insert_file_area(
        &mut self,
        start_addr: polyhal::VirtAddr,
        end_addr: polyhal::VirtAddr,
        permission: MapPermission,
        file: alloc::sync::Arc<dyn crate::fs::File>,
        offset: usize,
    ) -> bool {
        if !self.insert_framed_area(start_addr, end_addr, permission) {
            return false;
        }
        let area = self.areas.last_mut().unwrap();
        area.file = Some(file.clone());
        area.file_offset = offset;
        file.seek(offset);
        for vaddr in area.vaddr_range.clone() {
            let frame = match area.data_frames.get(&vaddr) {
                Some(frame) => frame,
                None => break,
            };
            let page = frame.paddr.slice_mut_with_len(crate::config::PAGE_SIZE);
            if file.read(page) <= 0 {
                break;
            }
        }
        true
    }

    pub fn insert_shared_frames(
        &mut self,
        start_addr: polyhal::VirtAddr,
        len: usize,
        frames: &[FrameTracker],
        permission: MapPermission,
    ) -> Option<polyhal::VirtAddr> {
        let len = (len + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
        if len == 0 || frames.len() < len / PAGE_SIZE {
            return None;
        }
        let start_addr = if start_addr.raw() == 0 {
            self.find_free_area(len)
        } else {
            start_addr
        };
        let end_addr = polyhal::VirtAddr::from(start_addr.raw() + len);
        if self.is_overlap(start_addr, end_addr) {
            return None;
        }
        let mut area = MapArea::new(start_addr, end_addr, MapType::Framed, permission);
        area.shared = true;
        for (idx, vpn) in area.vaddr_range.clone().into_iter().enumerate() {
            let frame = frames[idx].shared_clone();
            self.page_table
                .map_page(vpn, frame.paddr, permission.into(), MappingSize::Page4KB);
            area.data_frames.insert(vpn, frame);
        }
        self.areas.push(area);
        Some(start_addr)
    }

    pub fn msync(&mut self, start_addr: polyhal::VirtAddr, len: usize, flags: usize) -> isize {
        const MS_INVALIDATE: usize = 2;
        if start_addr.raw() % crate::config::PAGE_SIZE != 0 {
            return -22;
        }
        let end_addr = polyhal::VirtAddr::from(start_addr.raw().saturating_add(len));
        for area in self.areas.iter_mut() {
            let s = area.vaddr_range.get_start();
            let e = area.vaddr_range.get_end();
            if s >= end_addr || e <= start_addr {
                continue;
            }
            let ss = start_addr.max(s);
            let vpns: alloc::vec::Vec<VirtAddr> = area
                .vaddr_range
                .clone()
                .into_iter()
                .filter(|&v| v >= ss && v < end_addr)
                .collect();
            for vpn in vpns {
                if flags & MS_INVALIDATE != 0 {
                    self.page_table.unmap_page(vpn);
                }
            }
        }
        0
    }

    pub fn max_end_vaddr(&self) -> polyhal::VirtAddr {
        self.areas
            .iter()
            .map(|a| a.vaddr_range.get_end())
            .max()
            .unwrap_or_else(|| polyhal::VirtAddr::new(0))
    }

    pub fn munmap(&mut self, start_addr: polyhal::VirtAddr, end_addr: polyhal::VirtAddr) -> bool {
        let start_vaddr = start_addr.floor();
        let end_vaddr = end_addr.ceil();

        let mut unmapped_any = false;
        let mut i = 0;

        while i < self.areas.len() {
            let area_start = self.areas[i].vaddr_range.get_start();
            let area_end = self.areas[i].vaddr_range.get_end();

            if area_start < end_vaddr && area_end > start_vaddr {
                let unmap_start = start_vaddr.max(area_start);
                let unmap_end = end_vaddr.min(area_end);

                let vaddrs: alloc::vec::Vec<VirtAddr> = self.areas[i]
                    .data_frames
                    .range(unmap_start..unmap_end)
                    .map(|(&k, _)| k)
                    .collect();

                for vaddr in &vaddrs {
                    if self
                        .page_table
                        .translate(*vaddr)
                        .is_some_and(|(_, flags)| flags.contains(MappingFlags::P))
                    {
                        self.page_table.unmap_page(*vaddr);
                    }
                }
                for vaddr in &vaddrs {
                    self.areas[i].data_frames.remove(vaddr);
                }
                unmapped_any = true;

                if unmap_start <= area_start && unmap_end >= area_end {
                    self.areas.remove(i);
                } else if unmap_start <= area_start {
                    self.areas[i].vaddr_range = VAddrRange::new(unmap_end, area_end);
                    i += 1;
                } else if unmap_end >= area_end {
                    self.areas[i].vaddr_range = VAddrRange::new(area_start, unmap_start);
                    i += 1;
                } else {
                    let mut right = MapArea::from_another(&self.areas[i]);
                    right.vaddr_range = VAddrRange::new(unmap_end, area_end);
                    let right_keys: alloc::vec::Vec<VirtAddr> = self.areas[i]
                        .data_frames
                        .range(unmap_end..area_end)
                        .map(|(&k, _)| k)
                        .collect();
                    for key in right_keys {
                        if let Some(frame) = self.areas[i].data_frames.remove(&key) {
                            right.data_frames.insert(key, frame);
                        }
                    }
                    self.areas[i].vaddr_range = VAddrRange::new(area_start, unmap_start);
                    self.areas.insert(i + 1, right);
                    i += 1;
                }
            } else {
                i += 1;
            }
        }

        unmapped_any
    }
}

pub struct MapArea {
    pub vaddr_range: VAddrRange,
    data_frames: BTreeMap<VirtAddr, FrameTracker>,
    map_type: MapType,
    map_perm: MapPermission,
    file: Option<alloc::sync::Arc<dyn crate::fs::File>>,
    file_offset: usize,
    shared: bool,
    dirty: alloc::collections::BTreeSet<VirtAddr>,
}

impl MapArea {
    pub fn set_perm(&mut self, perm: MapPermission) {
        self.map_perm = perm;
    }
    pub fn new(
        start_va: VirtAddr,
        end_va: VirtAddr,
        map_type: MapType,
        map_perm: MapPermission,
    ) -> Self {
        let start_vpn: VirtAddr = start_va.floor();
        let end_vpn: VirtAddr = end_va.ceil();
        Self {
            vaddr_range: VAddrRange::new(start_vpn, end_vpn),
            data_frames: BTreeMap::new(),
            map_type,
            map_perm,
            file: None,
            file_offset: 0,
            shared: false,
            dirty: alloc::collections::BTreeSet::new(),
        }
    }
    pub fn from_another(another: &MapArea) -> Self {
        Self {
            vaddr_range: VAddrRange::new(
                another.vaddr_range.get_start(),
                another.vaddr_range.get_end(),
            ),
            data_frames: BTreeMap::new(),
            map_type: another.map_type,
            map_perm: another.map_perm,
            file: another.file.clone(),
            file_offset: another.file_offset,
            shared: another.shared,
            dirty: alloc::collections::BTreeSet::new(),
        }
    }
    pub fn map(&mut self, page_table: &Arc<PageTableWrapper>) -> Result<(), ()> {
        trace!("os::mm::memory_set::MapArea::map");
        for vaddr in self.vaddr_range {
            let p_tracker = match frame_alloc() {
                Some(frame) => frame,
                None => {
                    self.unmap(page_table);
                    self.data_frames.clear();
                    return Err(());
                }
            };
            page_table.map_page(
                vaddr,
                p_tracker.paddr,
                self.map_perm.into(),
                MappingSize::Page4KB,
            );
            self.data_frames.insert(vaddr, p_tracker);
        }
        Ok(())
    }

    /// Unmap page area
    #[allow(unused)]
    pub fn unmap(&mut self, page_table: &Arc<PageTableWrapper>) {
        trace!("os::mm::memory_set::MapArea::unmap");
        for vpn in self.vaddr_range {
            if page_table
                .translate(vpn)
                .is_some_and(|(_, flags)| flags.contains(MappingFlags::P))
            {
                page_table.unmap_page(vpn);
            }
        }
    }

    /// data: start-aligned but maybe with shorter length
    /// assume that all frames were cleared before
    /// page_off: offset within the first page where data should start
    pub fn copy_data(&mut self, _page_table: &Arc<PageTableWrapper>, data: &[u8], page_off: usize) {
        trace!("os::mm::memory_set::MapArea::copy_data");
        assert_eq!(self.map_type, MapType::Framed);
        let mut data_pos: usize = 0;
        let mut curr_vaddr = self.vaddr_range.get_start();
        let mut page_off = page_off;
        let len = data.len();

        while data_pos < len {
            let chunk = (PAGE_SIZE - page_off).min(len - data_pos);
            let src = &data[data_pos..data_pos + chunk];
            let frame = self.data_frames.get(&curr_vaddr).unwrap();
            let dst = &mut (frame.paddr + page_off).slice_mut_with_len(chunk);
            dst.copy_from_slice(src);
            data_pos += chunk;
            curr_vaddr = curr_vaddr + PAGE_SIZE;
            page_off = 0;
        }
    }

    fn copy_file_data(
        &mut self,
        file: &dyn File,
        mut file_offset: usize,
        file_size: usize,
        page_off: usize,
    ) -> bool {
        trace!("os::mm::memory_set::MapArea::copy_file_data");
        assert_eq!(self.map_type, MapType::Framed);
        let mut copied = 0usize;
        let mut curr_vaddr = self.vaddr_range.get_start();
        let mut page_off = page_off;

        while copied < file_size {
            let chunk = (PAGE_SIZE - page_off).min(file_size - copied);
            let Some(frame) = self.data_frames.get(&curr_vaddr) else {
                return false;
            };
            let dst = &mut (frame.paddr + page_off).slice_mut_with_len(chunk);
            if !read_exact_at(file, file_offset, dst) {
                return false;
            }
            copied += chunk;
            file_offset += chunk;
            curr_vaddr = curr_vaddr + PAGE_SIZE;
            page_off = 0;
        }
        true
    }
}

#[derive(Copy, Clone, PartialEq, Debug)]
pub enum MapType {
    //  Identical, not used now
    Framed,
}

bitflags! {
    pub struct MapPermission: u8 {
        const R = 1 << 1;
        const W = 1 << 2;
        const X = 1 << 3;
        const U = 1 << 4;
    }
}

impl Into<MappingFlags> for MapPermission {
    fn into(self) -> MappingFlags {
        let mut flags = MappingFlags::empty();
        if self.contains(MapPermission::R) {
            flags |= MappingFlags::R;
        }
        if self.contains(MapPermission::W) {
            flags |= MappingFlags::W;
        }
        if self.contains(MapPermission::X) {
            flags |= MappingFlags::X;
        }
        if self.contains(MapPermission::U) {
            flags |= MappingFlags::U;
        }
        flags
    }
}
