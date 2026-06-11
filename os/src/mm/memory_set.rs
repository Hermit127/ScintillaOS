use super::vpn_range::VAddrRange;
use super::{frame_alloc, FrameTracker};
use crate::config::{PAGE_SIZE, USER_STACK_SIZE};
use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;
use log::*;
use polyhal::pagetable::{MappingFlags, MappingSize, PageTable, PageTableWrapper};
use polyhal::{PhysAddr, VirtAddr};
pub struct MemorySet {
    page_table: Arc<PageTableWrapper>,
    areas: Vec<MapArea>,
}

pub struct LoadedElf {
    pub memory_set: MemorySet,
    pub user_sp: usize,
    pub entry_point: usize,
    pub ph_addr: usize,
    pub ph_entsize: usize,
    pub ph_num: usize,
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
        let mut max_end_va = VirtAddr::new(0);
        let mut ph_addr = 0usize;
        let mut first_load = true;
        for i in 0..ph_count {
            let ph = elf.program_header(i).unwrap();
            if ph.get_type().unwrap() == xmas_elf::program::Type::Load {
                let start_va: VirtAddr = (ph.virtual_addr() as usize).into();
                let end_va: VirtAddr = ((ph.virtual_addr() + ph.mem_size()) as usize).into();
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
            }
        }
        // map user stack with U flags
        let mut user_stack_bottom: usize = max_end_va.into();
        // guard page
        user_stack_bottom += PAGE_SIZE;
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
            entry_point: elf.header.pt2.entry_point() as usize,
            ph_addr,
            ph_entsize: e_phentsize,
            ph_num: ph_count as usize,
        }
    }
    pub fn from_existed_user(user_space: &MemorySet) -> Option<MemorySet> {
        Self::from_existed_user_shared(user_space, false)
    }

    pub fn clone_shared(user_space: &MemorySet) -> MemorySet {
        Self::from_existed_user_shared(user_space, true).unwrap()
    }

    fn from_existed_user_shared(user_space: &MemorySet, shared: bool) -> Option<MemorySet> {
        trace!("os::mm::MemorySet::from_existed_user");
        let mut memory_set = Self::new_bare();
        if shared {
            memory_set.page_table = user_space.page_table.clone();
        }
        for area in user_space.areas.iter() {
            let mut new_area = MapArea::from_another(area);
            for (&vpn, frame) in area.data_frames.iter() {
                if !shared {
                    if area.is_shared {
                        let shared_frame = frame.shared_clone();
                        memory_set.page_table.map_page(
                            vpn,
                            shared_frame.paddr,
                            area.map_perm.into(),
                            MappingSize::Page4KB,
                        );
                        new_area.data_frames.insert(vpn, shared_frame);
                    } else {
                        let new_frame = frame_alloc()?;
                        memory_set.page_table.map_page(
                            vpn,
                            new_frame.paddr,
                            area.map_perm.into(),
                            MappingSize::Page4KB,
                        );
                        unsafe {
                            core::ptr::copy_nonoverlapping(
                                frame.paddr.get_ptr::<u8>(),
                                new_frame.paddr.get_mut_ptr(),
                                PAGE_SIZE,
                            );
                        }
                        new_area.data_frames.insert(vpn, new_frame);
                    }
                } else {
                    new_area.data_frames.insert(vpn, frame.shared_clone());
                }
            }
            memory_set.areas.push(new_area);
        }
        Some(memory_set)
    }

    pub fn handle_page_fault(&mut self, vaddr: polyhal::VirtAddr) -> bool {
        let vpn = vaddr.floor();
        let area = match self.areas.iter_mut().find(|a| {
            a.vaddr_range.get_start() <= vpn && vpn < a.vaddr_range.get_end()
        }) {
            Some(a) => a,
            None => return false,
        };
        let translated = self.page_table.translate(vpn);
        let in_frames = area.data_frames.contains_key(&vpn);
        if !in_frames {
            let f = match frame_alloc() { Some(f) => f, None => return false };
            if let Some(ref file) = area.file {
                let off = area.file_offset + (vpn.raw() - area.vaddr_range.get_start().raw());
                let buf = f.paddr.slice_mut_with_len(crate::config::PAGE_SIZE);
                let _ = file.read_at(off, buf);
            }
            if self.page_table.translate(vpn).is_some() {
                self.page_table.unmap_page(vpn);
            }
            self.page_table.map_page(vpn, f.paddr, area.map_perm.into(), MappingSize::Page4KB);
            area.data_frames.insert(vpn, f);
            return true;
        }
        if translated.is_none() {
            let frame = area.data_frames.get(&vpn).unwrap();
            if self.page_table.translate(vpn).is_some() {
                self.page_table.unmap_page(vpn);
            }
            self.page_table.map_page(vpn, frame.paddr, area.map_perm.into(), MappingSize::Page4KB);
            return true;
        }
        // COW: writable area, page exists, shared frame
        if !area.map_perm.contains(MapPermission::W) { return false; }
        let (paddr, _) = translated.unwrap();
        let old = area.data_frames.get(&vpn).unwrap();
        if old.is_shared() {
            if area.is_shared {
                self.page_table.unmap_page(vpn);
                self.page_table.map_page(vpn, paddr, area.map_perm.into(), MappingSize::Page4KB);
            } else {
                let f = match frame_alloc() { Some(f) => f, None => return false };
                unsafe { core::ptr::copy_nonoverlapping(paddr.get_ptr::<u8>(), f.paddr.get_mut_ptr(), PAGE_SIZE); }
                self.page_table.unmap_page(vpn);
                self.page_table.map_page(vpn, f.paddr, area.map_perm.into(), MappingSize::Page4KB);
                area.data_frames.insert(vpn, f);
            }
        } else {
            self.page_table.unmap_page(vpn);
            self.page_table.map_page(vpn, paddr, area.map_perm.into(), MappingSize::Page4KB);
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
            let Some(area) = self
                .areas
                .iter()
                .find(|area| {
                    area.vaddr_range.get_start() <= vaddr && vaddr < area.vaddr_range.get_end()
                })
            else {
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
        //*self = Self::new_bare();
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
        is_shared: bool,
    ) -> bool {
        let mut map_area = MapArea::new(start_addr, end_addr, MapType::Framed, permission);
        map_area.file = Some(file);
        map_area.file_offset = offset;
        map_area.is_shared = is_shared;
        self.areas.push(map_area);
        true
    }

    pub fn msync(&mut self, start_addr: polyhal::VirtAddr, len: usize, flags: usize) -> isize {
        const MS_INVALIDATE: usize = 2;
        if start_addr.raw() % crate::config::PAGE_SIZE != 0 { return -22; }
        let end_addr = polyhal::VirtAddr::from(start_addr.raw().saturating_add(len));
        for area in self.areas.iter_mut() {
            let s = area.vaddr_range.get_start(); let e = area.vaddr_range.get_end();
            if s >= end_addr || e <= start_addr { continue; }
            let ss = start_addr.max(s);
            let vpns: alloc::vec::Vec<VirtAddr> = area.vaddr_range.clone().into_iter()
                .filter(|&v| v >= ss && v < end_addr).collect();
            for vpn in vpns {
                if flags & MS_INVALIDATE != 0 { self.page_table.unmap_page(vpn); }
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
    pub map_perm: MapPermission,
    file: Option<alloc::sync::Arc<dyn crate::fs::File>>,
    file_offset: usize,
    dirty: alloc::collections::BTreeSet<VirtAddr>,
    pub is_shared: bool,
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
            dirty: alloc::collections::BTreeSet::new(),
            is_shared: false,
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
            dirty: alloc::collections::BTreeSet::new(),
            is_shared: another.is_shared,
        }
    }
    pub fn map(&mut self, page_table: &Arc<PageTableWrapper>) -> Result<(), ()> {
        trace!("os::mm::memory_set::MapArea::map");
        for vaddr in self.vaddr_range {
            let p_tracker = frame_alloc().ok_or(())?;
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
            page_table.unmap_page(vpn);
        }
    }

    /// data: start-aligned but maybe with shorter length
    /// assume that all frames were cleared before
    /// page_off: offset within the first page where data should start
    pub fn copy_data(&mut self, page_table: &Arc<PageTableWrapper>, data: &[u8], page_off: usize) {
        trace!("os::mm::memory_set::MapArea::copy_data");
        assert_eq!(self.map_type, MapType::Framed);
        let mut data_pos: usize = 0;
        let mut curr_vaddr = self.vaddr_range.get_start();
        let len = data.len();

        if page_off > 0 {
            let first_chunk = (PAGE_SIZE - page_off).min(len);
            let dst = &mut page_table
                .translate((curr_vaddr.raw() + page_off).into())
                .unwrap()
                .0
                .slice_mut_with_len(first_chunk);
            dst.copy_from_slice(&data[..first_chunk]);
            data_pos = first_chunk;
            curr_vaddr = curr_vaddr + PAGE_SIZE;
        }

        while data_pos < len {
            let chunk = len - data_pos;
            let chunk = chunk.min(PAGE_SIZE);
            let src = &data[data_pos..data_pos + chunk];
            let dst = &mut page_table
                .translate(curr_vaddr.into())
                .unwrap()
                .0
                .slice_mut_with_len(chunk);
            dst.copy_from_slice(src);
            data_pos += chunk;
            curr_vaddr = curr_vaddr + PAGE_SIZE;
        }
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
