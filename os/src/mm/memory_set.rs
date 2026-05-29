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
    pub fn from_existed_user(user_space: &MemorySet) -> MemorySet {
        trace!("os::mm::MemorySet::from_existed_user (COW)");
        let mut memory_set = Self::new_bare();
        for area in user_space.areas.iter() {
            let mut new_area = MapArea::from_another(area);
            for vpn in area.vaddr_range {
                if let Some(frame) = area.data_frames.get(&vpn) {
                    let shared = frame.cow_clone();
                    let mut flags: MappingFlags = area.map_perm.into();
                    if area.map_perm.contains(MapPermission::W) {
                        flags.remove(MappingFlags::W);
                        let mut parent_flags: MappingFlags = area.map_perm.into();
                        parent_flags.remove(MappingFlags::W);
                        user_space.page_table.map_page(
                            vpn,
                            frame.paddr,
                            parent_flags,
                            MappingSize::Page4KB,
                        );
                    }
                    memory_set
                        .page_table
                        .map_page(vpn, shared.paddr, flags, MappingSize::Page4KB);
                    new_area.data_frames.insert(vpn, shared);
                }
            }
            memory_set.areas.push(new_area);
        }
        memory_set
    }
    pub fn activate(&self) {
        self.page_table.change();
    }
    pub fn translate(&self, vaddr: VirtAddr) -> Option<(PhysAddr, MappingFlags)> {
        self.page_table
            .translate(vaddr)
            .map(|(pa, flags)| (pa.into(), flags))
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
    ) {
        let mut map_area = MapArea::new(start_addr, end_addr, MapType::Framed, permission);
        map_area.map(&self.page_table);
        self.areas.push(map_area);
    }

    pub fn insert_file_area(
        &mut self,
        start_addr: polyhal::VirtAddr,
        end_addr: polyhal::VirtAddr,
        permission: MapPermission,
        file: alloc::sync::Arc<dyn crate::fs::File>,
        offset: usize,
    ) -> bool {
        self.insert_framed_area(start_addr, end_addr, permission);

        file.seek(offset);
        let area = self.areas.last().unwrap();

        for vaddr in area.vaddr_range.clone() {
            let (paddr, _flags) = self.page_table.translate(vaddr).unwrap();

            let page_content = paddr.slice_mut_with_len(crate::config::PAGE_SIZE);

            let read_len = file.read(page_content);
            if read_len == 0 {
                break;
            }
        }
        true
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

                if !vaddrs.is_empty() {
                    for vaddr in &vaddrs {
                        self.page_table.unmap_page(*vaddr);
                    }
                    for vaddr in &vaddrs {
                        self.areas[i].data_frames.remove(vaddr);
                    }
                    unmapped_any = true;
                }

                if self.areas[i].data_frames.is_empty() {
                    self.areas.remove(i);
                } else {
                    let min_vaddr = *self.areas[i].data_frames.keys().min().unwrap();
                    let max_vaddr = *self.areas[i].data_frames.keys().max().unwrap();
                    self.areas[i].vaddr_range =
                        VAddrRange::new(min_vaddr, max_vaddr + crate::config::PAGE_SIZE);
                    i += 1;
                }
            } else {
                i += 1;
            }
        }

        unmapped_any
    }
}

pub fn handle_cow_fault(bad_addr: usize) -> bool {
    use crate::mm::frame_alloc;
    use crate::task::current_task;
    let task = match current_task() {
        Some(t) => t,
        None => return false,
    };
    let mut inner = task.inner_exclusive_access();
    let vaddr: VirtAddr = bad_addr.into();
    let mem = &mut inner.memory_set;

    for area in mem.areas.iter_mut() {
        if vaddr < area.vaddr_range.get_start() || vaddr >= area.vaddr_range.get_end() {
            continue;
        }
        let vpn = vaddr.floor();
        if !area.map_perm.contains(MapPermission::W) {
            continue;
        }
        if let Some(frame) = area.data_frames.get(&vpn) {
            let refs = frame.ref_count();
            if refs == 0 {
                panic!(
                    "COW refcount=0 at paddr={:#x} vpn={:#x}",
                    frame.paddr.raw(),
                    vpn.raw()
                );
            }
            if refs <= 1 {
                let flags: MappingFlags = area.map_perm.into();
                mem.page_table
                    .map_page(vpn, frame.paddr, flags, MappingSize::Page4KB);
                return true;
            }
            let new_frame = match frame_alloc() {
                Some(f) => f,
                None => return false,
            };
            unsafe {
                core::ptr::copy_nonoverlapping(
                    frame.paddr.get_ptr::<u8>(),
                    new_frame.paddr.get_mut_ptr(),
                    PAGE_SIZE,
                );
            }
            let flags: MappingFlags = area.map_perm.into();
            mem.page_table
                .map_page(vpn, new_frame.paddr, flags, MappingSize::Page4KB);
            area.data_frames.insert(vpn, new_frame);
            return true;
        }
        continue;
    }
    false
}

pub struct MapArea {
    pub vaddr_range: VAddrRange,
    data_frames: BTreeMap<VirtAddr, FrameTracker>,
    map_type: MapType,
    map_perm: MapPermission,
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
