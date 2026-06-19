// 这里负责操作系统的内存管理：实现用户地址空间、ELF 装载、mmap/munmap 和按需缺页处理。
use super::vpn_range::VAddrRange;
use super::{frame_alloc, FrameTracker};
use crate::config::{PAGE_SIZE, USER_STACK_SIZE};
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

// 从 ELF 的 PT_INTERP 段里取动态链接器路径。
// 执行时先解析 ELF 头，再遍历程序头表；遇到 PT_INTERP 后按段偏移取出字符串，去掉末尾的 '\0' 再返回。
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

impl MemorySet {
    // 创建一个空地址空间，只带新的页表。
    // 执行时分配一份新的页表包装对象，并把映射区域列表初始化为空，供 exec 或 fork 后续继续填充。
    pub fn new_bare() -> Self {
        Self {
            page_table: Arc::new(PageTableWrapper::alloc()),
            areas: Vec::new(),
        }
    }
    // 返回当前地址空间的页表 token。
    // 执行时只取出内部页表句柄，调度切换和用户指针翻译会拿这个值访问当前地址空间。
    pub fn token(&self) -> PageTable {
        self.page_table.0
    }
    // 插入一个映射区域，并可选地把 ELF 数据拷到新页里。
    // 执行时先逐页建立映射；如果调用者带了初始数据，就按 page_off 写进新页，最后把区域记录到 areas 里。
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
    // 从 ELF 文件构造用户地址空间，同时建好用户栈和 program break。
    // 执行时解析程序头，按 PT_LOAD 权限映射代码和数据段；同时记录 PHDR、TLS 和解释器路径，再补上用户栈。
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

    // 把动态链接器 ELF 装到固定基址，返回它的入口和基址。
    // 执行时按 base 平移解释器的每个 PT_LOAD 段，建立映射并拷入文件内容，入口地址也按同一个基址修正。
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

    // fork 时复制父进程地址空间，写映射页按 COW 方式共享。
    // 执行时逐个复制 MapArea 元数据；普通可写页去掉父子页表的写权限并共享 FrameTracker，等写缺页时再复制。
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

    // 处理用户缺页：按需分配、文件回填，或者完成 COW。
    // 执行时先定位缺页地址对应的映射区域，再分配或复制物理页，最后更新页表。
    pub fn handle_page_fault(&mut self, vaddr: polyhal::VirtAddr, write_fault: bool) -> bool {
        let vpn = vaddr.floor();
        // 先找到缺页地址所属的 MapArea；没有对应区域就是非法访问。
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
            // 第一次访问这一页时才分配物理页，文件映射还要读入初始内容。
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
        // 只读区域上的写缺页应该交给上层转成 SIGSEGV。
        if write_fault && !area.map_perm.contains(MapPermission::W) {
            return false;
        }
        // 只读页已经有数据帧时，只需要把页表项补回来。
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
        // 可写页已经有数据帧时，后面只需要处理 COW 或恢复写权限。
        let (paddr, _) = match self.page_table.translate(vpn) {
            Some(x) => x,
            None => return false,
        };
        let old = match area.data_frames.get(&vpn) {
            Some(f) => f,
            None => return false,
        };
        if old.is_shared() {
            // 共享页被写时复制一份新物理页，避免改到父子进程共同数据。
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
            // 页已经是独占的，只需要按当前权限重新建立页表项。
            self.page_table.unmap_page(vpn);
            self.page_table
                .map_page(vpn, old.paddr, area.map_perm.into(), MappingSize::Page4KB);
        }
        true
    }
    // 创建共享页表视图，clone(CLONE_VM) 会用到。
    // 执行时直接复用原来的页表 Arc，不复制 areas；共享地址空间的线程因此看到同一份页表内容。
    pub fn from_shared(user_space: &MemorySet) -> MemorySet {
        MemorySet {
            page_table: user_space.page_table.clone(),
            areas: Vec::new(),
        }
    }
    // 切换硬件页表到当前地址空间。
    // 执行时调用底层页表包装对象的 change，让后续用户态访存走这份地址空间。
    pub fn activate(&self) {
        self.page_table.change();
    }
    // 查询虚拟地址对应的物理地址和映射权限。
    // 执行时委托页表做地址翻译，并把 polyhal 返回的物理地址转换成内核统一使用的 PhysAddr。
    pub fn translate(&self, vaddr: VirtAddr) -> Option<(PhysAddr, MappingFlags)> {
        self.page_table
            .translate(vaddr)
            .map(|(pa, flags)| (pa.into(), flags))
    }
    // 检查一段用户缓冲区是否完整映射，且满足读写权限要求。
    // 执行时先防止地址加法溢出，再逐页确认它属于用户映射、权限匹配，并且页表里确实有映射。
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
    // 清空所有用户映射，让 FrameTracker 自动回收物理页。
    // 执行时只清掉 areas，里面保存的 FrameTracker 会随之 drop；页表对象本身留给地址空间生命周期管理。
    pub fn recycle_data_pages(&mut self) {
        //*self = Self::new_bare();
        self.areas.clear();
    }
    // 删除起点不低于 limit_va 的映射区域。
    // 执行时遍历 areas，把高地址区域从页表里 unmap 后移除，低于 limit 的区域继续保留。
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
    // 判断给定虚拟地址区间是否和已有映射重叠。
    // 执行时遍历现有 MapArea，只要新区间和任意旧区间有交集就返回 true。
    pub fn is_overlap(&self, start_addr: polyhal::VirtAddr, end_addr: polyhal::VirtAddr) -> bool {
        for area in self.areas.iter() {
            if start_addr < area.vaddr_range.get_end() && end_addr > area.vaddr_range.get_start() {
                return true;
            }
        }
        false
    }

    // 从低地址开始找一段足够大的空闲用户虚拟区间。
    // 执行时先把现有映射按起点排序，再用 cursor 扫描相邻空洞，找到第一段能容纳 len 的页对齐区间。
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

    // 返回页表包装对象，少数系统调用需要直接改页表。
    // 执行时暴露内部 Arc 引用，调用者可以在不搬走 MemorySet 的情况下继续操作页表。
    pub fn page_table_mut(&mut self) -> &Arc<PageTableWrapper> {
        &self.page_table
    }
    // 返回映射区域列表的可变引用。
    // 执行时把 areas 交给 mmap、brk 等路径做局部调整，调用方需要自己保持区域不重叠。
    pub fn areas_mut(&mut self) -> &mut Vec<MapArea> {
        &mut self.areas
    }

    // 插入匿名映射，并立即分配物理页。
    // 执行时构造 MapArea、逐页分配物理页并写页表；成功后把区域挂进当前地址空间。
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

    // 插入文件映射，并把已有文件内容预读进映射页。
    // 执行时先建匿名页，再记录文件对象和文件偏移；随后从文件当前内容读入每个已分配页面。
    pub fn insert_file_area(
        &mut self,
        start_addr: polyhal::VirtAddr,
        end_addr: polyhal::VirtAddr,
        permission: MapPermission,
        file: alloc::sync::Arc<dyn crate::fs::File>,
        offset: usize,
    ) -> bool {
        self.insert_framed_area(start_addr, end_addr, permission);
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

    // 把一组已有物理页映射到当前地址空间，供 SysV shm 使用。
    // 执行时校验长度和 frame 数量，必要时自动找空洞，然后把每个共享 FrameTracker 映射到连续虚拟页。
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

    // 处理 msync；当前主要支持按需失效页表项。
    // 执行时先检查起始地址页对齐，再找出和目标区间相交的区域；带 MS_INVALIDATE 时逐页撤掉页表项。
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

    // 返回所有映射里最大的结束地址。
    // 执行时遍历 areas 的结束虚拟地址，取最大值；没有任何映射时返回 0。
    pub fn max_end_vaddr(&self) -> polyhal::VirtAddr {
        self.areas
            .iter()
            .map(|a| a.vaddr_range.get_end())
            .max()
            .unwrap_or_else(|| polyhal::VirtAddr::new(0))
    }

    // 解除一段映射，必要时把原 MapArea 切成左右两段。
    // 执行时先计算虚拟地址范围和权限，再修改页表与映射区域记录，最后返回映射处理结果。
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
    // 更新这段映射的权限记录。
    // 执行时只修改 MapArea 保存的权限位，真正的页表权限由调用路径在重建映射时同步。
    pub fn set_perm(&mut self, perm: MapPermission) {
        self.map_perm = perm;
    }
    // 创建一段页对齐后的映射区域描述。
    // 执行时把传入地址向页边界取整，初始化数据页表、文件来源和共享标记，暂时不分配物理页。
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
    // 复制映射区域的元数据，但不复制物理页。
    // 执行时保留地址范围、权限、文件来源和共享属性，data_frames 重新置空，供 fork 后重新挂共享页。
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
    // 给这段区域逐页分配物理页并写入页表。
    // 执行时先计算虚拟地址范围和权限，再修改页表与映射区域记录，最后返回映射处理结果。
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

    // 从页表中解除这段区域的映射。
    #[allow(unused)]
    // 执行时遍历区域内每个虚拟页，从传入页表删除对应页表项；物理页由 data_frames 的所有权负责释放。
    pub fn unmap(&mut self, page_table: &Arc<PageTableWrapper>) {
        trace!("os::mm::memory_set::MapArea::unmap");
        for vpn in self.vaddr_range {
            page_table.unmap_page(vpn);
        }
    }

    // 把文件或 ELF 段数据拷进已经清零的物理页，首个页内偏移由 page_off 指定。
    // 执行时按页切分 data，第一页从 page_off 写起，后续页从页首继续写，直到把输入字节全部拷完。
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
}

#[derive(Copy, Clone, PartialEq, Debug)]
pub enum MapType {
    // 普通页
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
    // 转成 polyhal 页表层使用的权限位。
    // 执行时逐位检查 R/W/X/U 标记，并拼成底层 map_page 接口需要的 MappingFlags。
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
