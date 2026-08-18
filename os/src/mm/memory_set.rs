use super::vpn_range::VAddrRange;
use super::{
    file_page_cache::load_file_page, frame_alloc, frames_dealloc_batch, FrameTracker,
};
use crate::config::{PAGE_SIZE, USER_STACK_SIZE};
use crate::fs::File;
use crate::sync::{VmPageTableRwLock, VmPagesGuard, VmPagesLock, VmRwLock};
use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicUsize, Ordering};
use lazy_static::lazy_static;
use log::*;
use polyhal::pagetable::{MappingFlags, MappingSize, PageTable, PageTableWrapper};
use polyhal::{PhysAddr, VirtAddr};
use xmas_elf::header::Type as ElfType;
struct AddressSpacePageTable {
    inner: PageTableWrapper,
    access: VmPageTableRwLock<()>,
}

impl AddressSpacePageTable {
    fn alloc() -> Self {
        Self {
            inner: PageTableWrapper::alloc(),
            access: VmPageTableRwLock::new_named((), "Vm.page_table"),
        }
    }

    fn map_page(
        &self,
        vaddr: VirtAddr,
        paddr: PhysAddr,
        flags: MappingFlags,
        size: MappingSize,
    ) {
        let _access = self.access.write();
        self.inner.map_page(vaddr, paddr, flags, size);
    }

    fn map_pages_batch(&self, mappings: &[(VirtAddr, PhysAddr, MappingFlags)]) {
        let _access = self.access.write();
        self.inner.map_pages_batch(mappings);
    }

    fn unmap_page(&self, vaddr: VirtAddr) {
        let _access = self.access.write();
        self.inner.unmap_page(vaddr);
    }

    fn unmap_pages_batch(&self, pages: &[VirtAddr]) {
        let _access = self.access.write();
        self.inner.unmap_pages_batch(pages);
    }

    fn unmap_pages_if_match_batch(&self, pages: &[(VirtAddr, PhysAddr)]) {
        let _access = self.access.write();
        self.inner.unmap_pages_if_match_batch(pages);
    }

    fn translate(&self, vaddr: VirtAddr) -> Option<(PhysAddr, MappingFlags)> {
        let _access = self.access.read();
        self.inner.translate(vaddr)
    }

    fn update_permissions_batch(
        &self,
        updates: &[(VirtAddr, PhysAddr, MappingFlags)],
    ) -> bool {
        let _access = self.access.write();
        self.inner.update_pages_flags_batch(updates)
    }

    fn copy_from_user_page(&self, vaddr: VirtAddr, dst: &mut [u8]) -> bool {
        debug_assert!(dst.len() <= PAGE_SIZE - (vaddr.raw() & (PAGE_SIZE - 1)));
        let _access = self.access.read();
        let Some((paddr, flags)) = self.inner.translate(vaddr) else {
            return false;
        };
        if !flags.contains(MappingFlags::P)
            || (!flags.contains(MappingFlags::R) && !flags.contains(MappingFlags::W))
        {
            return false;
        }
        unsafe {
            core::ptr::copy_nonoverlapping(paddr.get_ptr::<u8>(), dst.as_mut_ptr(), dst.len());
        }
        true
    }

    fn copy_to_user_page(&self, vaddr: VirtAddr, src: &[u8]) -> bool {
        debug_assert!(src.len() <= PAGE_SIZE - (vaddr.raw() & (PAGE_SIZE - 1)));
        let _access = self.access.read();
        let Some((paddr, flags)) = self.inner.translate(vaddr) else {
            return false;
        };
        if !flags.contains(MappingFlags::P) || !flags.contains(MappingFlags::W) {
            return false;
        }
        unsafe {
            core::ptr::copy_nonoverlapping(src.as_ptr(), paddr.get_mut_ptr::<u8>(), src.len());
        }
        true
    }

    fn token(&self) -> PageTable {
        self.inner.0
    }

    fn change(&self) {
        self.inner.change();
    }
}

pub struct MemorySet {
    page_table: Arc<AddressSpacePageTable>,
    /// Mapping metadata belongs to the address space, not a task.  CLONE_VM
    /// therefore shares this lock and vector between every participating TCB
    /// instead of attempting to propagate eventually-consistent copies.
    areas: Arc<VmRwLock<Vec<MapArea>>>,
    shared_page_table: bool,
    /// CPUs whose SATP currently names this address space. Shared-VM tasks
    /// deliberately share this Arc so a later remote-TLB shootdown can target
    /// the complete address-space mask rather than a single TCB's metadata.
    activity: Arc<AddressSpaceActivity>,
    /// `brk` is part of the address-space state, even when CLONE_VM creates
    /// tasks with different TGIDs. Keep the authoritative value shared rather
    /// than treating each TCB copy as the source of truth.
    program_brk: Arc<AtomicUsize>,
}

/// Pages detached from an address space by `munmap`.
///
/// Unmapping a PTE and releasing its physical page are intentionally two
/// separate operations on SMP.  The caller must first invalidate the range
/// on every CPU recorded by `active_cpu_mask`, then call `release` after all
/// Task/Vm locks have been dropped.  Keeping the frame trackers here prevents
/// an allocator reuse while a remote hart could still have a stale TLB entry.
struct DeferredSharedWriteback {
    file: Arc<dyn File>,
    file_offset: usize,
    frame: FrameTracker,
}

impl DeferredSharedWriteback {
    fn flush(self) {
        let file_size = self.file.stat().st_size.max(0) as usize;
        if self.file_offset >= file_size {
            return;
        }
        let write_len = PAGE_SIZE.min(file_size - self.file_offset);
        let buffer = self.frame.paddr.slice_mut_with_len(write_len);
        let _ = self.file.write_at(self.file_offset, buffer);
    }
}

pub struct UnmapResult {
    unmapped: bool,
    retired_frames: Vec<FrameTracker>,
    writebacks: Vec<DeferredSharedWriteback>,
    retired_areas: Vec<MapArea>,
}

impl UnmapResult {
    fn new(
        unmapped: bool,
        retired_frames: Vec<FrameTracker>,
        writebacks: Vec<DeferredSharedWriteback>,
        retired_areas: Vec<MapArea>,
    ) -> Self {
        Self {
            unmapped,
            retired_frames,
            writebacks,
            retired_areas,
        }
    }

    pub fn unmapped(&self) -> bool {
        self.unmapped
    }

    /// Release the physical pages only after the corresponding local and
    /// remote TLB invalidations have completed.
    pub fn release(self) {
        for writeback in self.writebacks {
            writeback.flush();
        }
        drop(self.retired_frames);
        drop(self.retired_areas);
    }
}

/// Result of relocating or resizing a user mapping.
///
/// Frames removed by a shrinking remap stay owned here until the caller has
/// invalidated stale translations on every CPU that can use the address
/// space.  Moving a mapping transfers the same frame trackers to their new
/// virtual addresses, so its contents and copy-on-write state are retained.
pub struct MremapResult {
    new_addr: usize,
    old_mapping_changed: bool,
    retired_frames: Vec<FrameTracker>,
}

impl MremapResult {
    fn new(new_addr: usize, old_mapping_changed: bool, retired_frames: Vec<FrameTracker>) -> Self {
        Self {
            new_addr,
            old_mapping_changed,
            retired_frames,
        }
    }

    pub fn new_addr(&self) -> usize {
        self.new_addr
    }

    pub fn old_mapping_changed(&self) -> bool {
        self.old_mapping_changed
    }

    pub fn release(self) {
        drop(self.retired_frames);
    }
}

struct AddressSpaceActivity {
    active_cpus: AtomicUsize,
}

impl AddressSpaceActivity {
    const fn new() -> Self {
        Self {
            active_cpus: AtomicUsize::new(0),
        }
    }
}

/// A lock-free reference to the CPUs currently executing one address space.
///
/// TLB shootdown runs after the task and VM locks have been released, so it
/// must retain this state without borrowing a `MemorySet` guard.
#[derive(Clone)]
pub struct AddressSpaceActivityHandle(Arc<AddressSpaceActivity>);

impl AddressSpaceActivityHandle {
    pub fn active_cpu_mask(&self) -> usize {
        self.0.active_cpus.load(Ordering::Acquire)
    }
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

lazy_static! {
    static ref RETIRED_USER_PAGE_TABLE: Arc<AddressSpacePageTable> =
        Arc::new(AddressSpacePageTable::alloc());
}

fn retired_user_page_table() -> Arc<AddressSpacePageTable> {
    RETIRED_USER_PAGE_TABLE.clone()
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PageFaultResult {
    Handled,
    SharedMetadataChanged,
    SharedTranslationChanged,
}

pub const PAGE_FAULT_INVALID_ADDRESS: usize = 1;
pub const PAGE_FAULT_NO_AREA: usize = 2;
pub const PAGE_FAULT_PERMISSION: usize = 3;
pub const PAGE_FAULT_FRAME_ALLOC: usize = 4;
pub const PAGE_FAULT_METADATA: usize = 5;

static LAST_PAGE_FAULT_FAILURE: AtomicUsize = AtomicUsize::new(0);

fn page_fault_failure(reason: usize) -> Option<PageFaultResult> {
    LAST_PAGE_FAULT_FAILURE.store(reason, Ordering::Release);
    None
}

pub fn last_page_fault_failure() -> usize {
    LAST_PAGE_FAULT_FAILURE.load(Ordering::Acquire)
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PageFaultAccess {
    Read,
    Write,
    Execute,
}

struct PreparedFaultPage {
    file: Option<Arc<dyn File>>,
    file_offset: usize,
    cacheable: bool,
    frame: FrameTracker,
    copied: usize,
}

impl PreparedFaultPage {
    fn still_matches(&self, area: &MapArea, vpn: VirtAddr) -> bool {
        match (&self.file, &area.file) {
            (None, None) => true,
            (Some(expected), Some(current)) => {
                Arc::ptr_eq(expected, current)
                    && area.file_offset_at(vpn) == Some(self.file_offset)
                    && self.cacheable == !area.map_perm.contains(MapPermission::W)
            }
            _ => false,
        }
    }
}

struct PreparedCowPage {
    source_paddr: PhysAddr,
    _source: FrameTracker,
    frame: FrameTracker,
    shared_translation: bool,
}

struct ForkAreaSnapshot {
    start: VirtAddr,
    end: VirtAddr,
    frames: BTreeMap<VirtAddr, FrameTracker>,
    frame_set_id: usize,
    frame_generation: usize,
    map_type: MapType,
    map_perm: MapPermission,
    file: Option<Arc<dyn File>>,
    file_offset: usize,
    shared: bool,
}

impl ForkAreaSnapshot {
    fn capture(area: &MapArea) -> Self {
        let start = area.vaddr_range.get_start();
        let end = area.vaddr_range.get_end();
        let (frames, frame_generation) =
            area.data_frames.clone_shared_range_consistent(start, end);
        Self {
            start,
            end,
            frames,
            frame_set_id: area.data_frames.id(),
            frame_generation,
            map_type: area.map_type,
            map_perm: area.map_perm,
            file: area.file.clone(),
            file_offset: area.file_offset,
            shared: area.shared,
        }
    }

    fn matches(&self, area: &MapArea) -> bool {
        let same_file = match (&self.file, &area.file) {
            (None, None) => true,
            (Some(expected), Some(current)) => Arc::ptr_eq(expected, current),
            _ => false,
        };
        self.start == area.vaddr_range.get_start()
            && self.end == area.vaddr_range.get_end()
            && self.map_type == area.map_type
            && self.map_perm == area.map_perm
            && self.file_offset == area.file_offset
            && self.shared == area.shared
            && same_file
            && area.data_frames.id() == self.frame_set_id
            && area.data_frames.generation() == self.frame_generation
    }

    fn into_map_area(self) -> MapArea {
        let data_frames = ResidentFrames::new();
        data_frames.replace(self.frames);
        MapArea {
            vaddr_range: VAddrRange::new(self.start, self.end),
            data_frames,
            map_type: self.map_type,
            map_perm: self.map_perm,
            file: self.file,
            file_offset: self.file_offset,
            shared: self.shared,
            dirty: alloc::collections::BTreeSet::new(),
        }
    }
}

impl PreparedCowPage {
    fn new(source: FrameTracker, shared_translation: bool) -> Option<Self> {
        let source_paddr = source.paddr;
        let frame = frame_alloc()?;
        unsafe {
            core::ptr::copy_nonoverlapping(
                source_paddr.get_ptr::<u8>(),
                frame.paddr.get_mut_ptr(),
                PAGE_SIZE,
            );
        }
        Some(Self {
            source_paddr,
            _source: source,
            frame,
            shared_translation,
        })
    }

    fn still_matches(
        &self,
        area: &MapArea,
        frames: &BTreeMap<VirtAddr, FrameTracker>,
        vpn: VirtAddr,
        page_table: &AddressSpacePageTable,
    ) -> bool {
        let metadata_matches = frames
            .get(&vpn)
            .is_some_and(|frame| frame.paddr == self.source_paddr);
        let translation_matches = page_table.translate(vpn).is_some_and(|(paddr, flags)| {
            paddr.raw() == self.source_paddr.raw()
                && flags.contains(MappingFlags::P)
                && !flags.contains(MappingFlags::W)
        });
        metadata_matches
            && translation_matches
            && area.map_perm.contains(MapPermission::W)
            && (self.shared_translation || !area.shared)
    }
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
        let remaining = buf.len() - done;
        let n = file.read_at(offset, &mut buf[done..]);
        if n <= 0 || n as usize > remaining {
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
    pub(crate) fn fault_view(&self) -> Self {
        Self {
            page_table: Arc::clone(&self.page_table),
            areas: Arc::clone(&self.areas),
            shared_page_table: self.shared_page_table,
            activity: Arc::clone(&self.activity),
            program_brk: Arc::clone(&self.program_brk),
        }
    }

    pub fn new_bare() -> Self {
        Self {
            page_table: Arc::new(AddressSpacePageTable::alloc()),
            areas: Arc::new(VmRwLock::new_named(Vec::new(), "Vm.address_space_areas")),
            shared_page_table: false,
            activity: Arc::new(AddressSpaceActivity::new()),
            program_brk: Arc::new(AtomicUsize::new(0)),
        }
    }
    pub fn token(&self) -> PageTable {
        self.page_table.token()
    }
    pub fn mark_shared(&mut self) {
        self.shared_page_table = true;
    }

    pub fn is_shared(&self) -> bool {
        self.shared_page_table
    }

    /// Stable identity for all TCBs that execute the same page-table object.
    /// It is used only for peer discovery; page-table mutations themselves are
    /// serialized by the canonical `areas` VM lock.
    pub fn address_space_id(&self) -> usize {
        Arc::as_ptr(&self.page_table) as usize
    }

    #[cfg(feature = "smp-diagnostics")]
    pub fn diagnostic_identity(&self) -> (usize, usize, bool) {
        (
            Arc::as_ptr(&self.page_table) as usize,
            Arc::as_ptr(&self.areas) as usize,
            self.shared_page_table,
        )
    }

    pub fn program_brk(&self) -> usize {
        self.program_brk.load(Ordering::Acquire)
    }

    pub fn set_program_brk(&self, program_brk: usize) {
        self.program_brk.store(program_brk, Ordering::Release);
    }

    /// Publish that `cpu` may access user mappings through this page table.
    ///
    /// Scheduler callers set this after the page-table switch and before
    /// resuming any user-visible execution. The local switch invalidates its
    /// TLB, so a concurrent mapping change before publication cannot leave a
    /// stale user translation on this hart.
    pub fn enter_cpu(&self, cpu: usize) {
        assert!(
            cpu < usize::BITS as usize,
            "invalid address-space CPU {}",
            cpu
        );
        self.activity
            .active_cpus
            .fetch_or(1usize << cpu, Ordering::Release);
    }

    /// Remove `cpu` after it has switched back to the kernel page table.
    pub fn leave_cpu(&self, cpu: usize) {
        assert!(
            cpu < usize::BITS as usize,
            "invalid address-space CPU {}",
            cpu
        );
        self.activity
            .active_cpus
            .fetch_and(!(1usize << cpu), Ordering::Release);
    }

    /// Snapshot CPUs currently executing this page table. This is the target
    /// mask for the upcoming remote-TLB-shootdown phase.
    pub fn active_cpu_mask(&self) -> usize {
        self.activity.active_cpus.load(Ordering::Acquire)
    }

    /// Keep the address-space activity state alive across a remote TLB fence.
    pub fn activity_handle(&self) -> AddressSpaceActivityHandle {
        AddressSpaceActivityHandle(self.activity.clone())
    }

    pub fn shared_metadata_snapshot(&self) -> Vec<MapArea> {
        self.areas
            .read()
            .iter()
            .map(MapArea::from_another)
            .collect()
    }

    pub fn replace_shared_metadata(&mut self, areas: &[MapArea]) {
        // A shared address space now owns one canonical metadata vector, so
        // peer propagation is deliberately a no-op.  Retaining this method
        // keeps existing callers source-compatible while preventing a peer
        // from replacing newer canonical state with an older snapshot.
        if self.shared_page_table {
            return;
        }
        let mut current = self.areas.exclusive_access();
        let old_areas = core::mem::take(&mut *current);
        *current = areas
            .iter()
            .map(|incoming| {
                let mut area = MapArea::from_another(incoming);
                let cloned_frames = incoming.data_frames.clone_shared_range(
                    incoming.vaddr_range.get_start(),
                    incoming.vaddr_range.get_end(),
                );
                area.data_frames.replace(cloned_frames);
                area
            })
            .collect();
        for old in old_areas.iter() {
            let old_frames = old.data_frames.clone_shared_range(
                old.vaddr_range.get_start(),
                old.vaddr_range.get_end(),
            );
            for (vpn, frame) in old_frames {
                if let Some(area) = current.iter_mut().find(|area| {
                    area.vaddr_range.get_start() <= vpn && vpn < area.vaddr_range.get_end()
                }) {
                    area.data_frames
                        .lock_page(vpn)
                        .entry(vpn)
                        .or_insert(frame);
                }
            }
        }
        self.shared_page_table = true;
        drop(current);
        drop(old_areas);
    }
    pub fn push(&mut self, mut map_area: MapArea, data: Option<&[u8]>, page_off: usize) -> bool {
        if map_area.map(&self.page_table).is_err() {
            return false;
        }
        if let Some(data) = data {
            map_area.copy_data(&self.page_table, data, page_off);
        }
        self.areas.exclusive_access().push(map_area);
        true
    }

    fn install_signal_return_trampoline(&mut self) -> bool {
        #[cfg(target_arch = "riscv64")]
        const CODE: [u8; 8] = [
            0x93, 0x08, 0xb0, 0x08, // addi a7, zero, __NR_rt_sigreturn (139)
            0x73, 0x00, 0x00, 0x00, // ecall
        ];
        #[cfg(target_arch = "loongarch64")]
        const CODE: [u8; 8] = [
            0x0b, 0x2c, 0x82, 0x03, // ori a7, zero, __NR_rt_sigreturn (139)
            0x00, 0x00, 0x2b, 0x00, // syscall 0
        ];
        #[cfg(not(any(target_arch = "riscv64", target_arch = "loongarch64")))]
        const CODE: [u8; 0] = [];

        let start = crate::config::SIGNAL_RETURN_TRAMPOLINE;
        self.push(
            MapArea::new(
                start.into(),
                (start + PAGE_SIZE).into(),
                MapType::Framed,
                MapPermission::R | MapPermission::X | MapPermission::U,
            ),
            Some(&CODE),
            0,
        )
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
        #[cfg(feature = "smp-diagnostics")]
        {
            let relocation_page: VirtAddr = 0x15_8000usize.into();
            if map_area.vaddr_range.get_start() <= relocation_page
                && relocation_page < map_area.vaddr_range.get_end()
            {
                if let Some(frame) = map_area.data_frames.get_shared(relocation_page) {
                    let head = unsafe { core::ptr::read_unaligned(frame.paddr.get_ptr() as *const u32) };
                    let relocation_info = unsafe {
                        core::ptr::read_unaligned((frame.paddr + 0x404).get_ptr() as *const u32)
                    };
                    let source_offset = file_offset
                        + relocation_page.raw()
                        - map_area.vaddr_range.get_start().raw()
                        + 0x404;
                    let mut source = [0u8; core::mem::size_of::<u32>()];
                    let read = file.read_at(source_offset, &mut source);
                    crate::println!(
                        "[elf-diag] relocation source off={:#x} read={} source={:#010x} loaded={:#010x}",
                        source_offset,
                        read,
                        u32::from_le_bytes(source),
                        relocation_info,
                    );
                    crate::mm::watch_frame(frame.paddr, 0x404, head, relocation_info);
                }
            }
        }
        self.areas.exclusive_access().push(map_area);
        true
    }

    fn push_lazy_file_segment(
        &mut self,
        start_va: VirtAddr,
        end_va: VirtAddr,
        permission: MapPermission,
        file: Arc<dyn File>,
        file_offset: usize,
        file_size: usize,
    ) -> bool {
        let start = start_va.raw();
        let end = end_va.raw();
        if end < start || file_size > end - start {
            return false;
        }
        if end == start {
            return true;
        }
        let aligned_start = start / PAGE_SIZE * PAGE_SIZE;
        let Some(aligned_end) = end
            .checked_add(PAGE_SIZE - 1)
            .map(|value| value / PAGE_SIZE * PAGE_SIZE)
        else {
            return false;
        };
        if aligned_start == aligned_end {
            return true;
        }

        let page_offset = start - aligned_start;
        let mut copied = 0usize;
        let mut cursor = aligned_start;
        if page_offset != 0 && file_size != 0 {
            let head_len = (PAGE_SIZE - page_offset).min(file_size);
            if !self.push_file_segment(
                MapArea::new(
                    aligned_start.into(),
                    (aligned_start + PAGE_SIZE).into(),
                    MapType::Framed,
                    permission,
                ),
                file.as_ref(),
                file_offset,
                head_len,
                page_offset,
            ) {
                return false;
            }
            copied = head_len;
            cursor += PAGE_SIZE;
        }

        let full_len = (file_size - copied) / PAGE_SIZE * PAGE_SIZE;
        if full_len != 0 {
            let Some(full_offset) = file_offset.checked_add(copied) else {
                return false;
            };
            if !self.insert_file_area(
                cursor.into(),
                (cursor + full_len).into(),
                permission,
                file.clone(),
                full_offset,
            ) {
                return false;
            }
            copied += full_len;
            cursor += full_len;
        }

        let tail_len = file_size - copied;
        if tail_len != 0 {
            let Some(tail_offset) = file_offset.checked_add(copied) else {
                return false;
            };
            if !self.push_file_segment(
                MapArea::new(
                    cursor.into(),
                    (cursor + PAGE_SIZE).into(),
                    MapType::Framed,
                    permission,
                ),
                file.as_ref(),
                tail_offset,
                tail_len,
                0,
            ) {
                return false;
            }
            cursor += PAGE_SIZE;
        }

        cursor >= aligned_end
            || self.insert_lazy_area(cursor.into(), aligned_end.into(), permission)
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
        assert!(
            memory_set.install_signal_return_trampoline(),
            "failed to map rt_sigreturn trampoline"
        );
        memory_set.set_program_brk(program_break);
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

    pub fn from_elf_file(file: Arc<dyn File>) -> Option<LoadedElf> {
        trace!("os::mm::MemorySet::from_elf_file");
        let header_data = match read_elf_header_table(file.as_ref()) {
            Some(data) => data,
            None => {
                #[cfg(feature = "smp-diagnostics")]
                crate::println!("[exec-diag] ELF file load failed: header read");
                return None;
            }
        };
        let elf = match xmas_elf::ElfFile::new(&header_data) {
            Ok(elf) => elf,
            Err(_) => {
                #[cfg(feature = "smp-diagnostics")]
                crate::println!("[exec-diag] ELF file load failed: header parse");
                return None;
            }
        };
        let elf_header = elf.header;
        if elf_header.pt1.magic != [0x7f, 0x45, 0x4c, 0x46] {
            #[cfg(feature = "smp-diagnostics")]
            crate::println!("[exec-diag] ELF file load failed: bad magic");
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
            let ph = match elf.program_header(i) {
                Ok(ph) => ph,
                Err(_) => {
                    #[cfg(feature = "smp-diagnostics")]
                    crate::println!("[exec-diag] ELF file load failed: bad program header {}", i);
                    return None;
                }
            };
            let ph_type = match ph.get_type() {
                Ok(kind) => kind,
                Err(_) => {
                    #[cfg(feature = "smp-diagnostics")]
                    crate::println!("[exec-diag] ELF file load failed: bad program type {}", i);
                    return None;
                }
            };
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
                max_end_va = max_end_va.max(end_va.ceil());
                if !memory_set.push_lazy_file_segment(
                    start_va,
                    end_va,
                    map_perm,
                    file.clone(),
                    ph.offset() as usize,
                    ph.file_size() as usize,
                ) {
                    #[cfg(feature = "smp-diagnostics")]
                    crate::println!(
                        "[exec-diag] ELF file load failed: segment {} offset={:#x} size={:#x}",
                        i,
                        ph.offset(),
                        ph.file_size(),
                    );
                    return None;
                }
            } else if ph_type == xmas_elf::program::Type::Tls {
                tls_base = ph.virtual_addr() as usize + va_offset;
                tls_size = ph.mem_size() as usize;
            } else if ph_type == xmas_elf::program::Type::Interp {
                let offset = ph.offset() as usize;
                let size = ph.file_size() as usize;
                let raw = match read_small_file_region(file.as_ref(), offset, size) {
                    Some(data) => data,
                    None => {
                        #[cfg(feature = "smp-diagnostics")]
                        crate::println!("[exec-diag] ELF file load failed: interp path");
                        return None;
                    }
                };
                let end = raw.iter().position(|&b| b == 0).unwrap_or(size);
                interp_path = core::str::from_utf8(&raw[..end]).ok().map(|s| s.into());
            }
        }

        let program_break = (max_end_va.raw() + PAGE_SIZE - 1) / PAGE_SIZE * PAGE_SIZE;
        let user_stack_top: usize = 0x8_0000_0000;
        let user_stack_bottom = user_stack_top - USER_STACK_SIZE;
        let user_stack_top = user_stack_bottom + USER_STACK_SIZE;
        if !memory_set.insert_lazy_area(
            user_stack_bottom.into(),
            user_stack_top.into(),
            MapPermission::R | MapPermission::W | MapPermission::U,
        ) {
            #[cfg(feature = "smp-diagnostics")]
            crate::println!("[exec-diag] ELF file load failed: user stack");
            return None;
        }
        if !memory_set.install_signal_return_trampoline() {
            #[cfg(feature = "smp-diagnostics")]
            crate::println!("[exec-diag] ELF file load failed: signal trampoline");
            return None;
        }
        memory_set.set_program_brk(program_break);
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
        file: Arc<dyn File>,
        base: usize,
    ) -> Option<(usize, usize)> {
        let header_data = read_elf_header_table(file.as_ref())?;
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
                if !memory_set.push_lazy_file_segment(
                    start_va,
                    end_va,
                    map_perm,
                    file.clone(),
                    ph.offset() as usize,
                    ph.file_size() as usize,
                ) {
                    return None;
                }
            }
        }
        Some((entry, base))
    }

    fn pte_permission_for_frame(area: &MapArea, frame: &FrameTracker) -> MapPermission {
        let mut perm = area.map_perm;
        if !area.shared && perm.contains(MapPermission::W) && frame.is_shared() {
            perm.remove(MapPermission::W);
        }
        perm
    }

    fn area_allows_access(area: &MapArea, access: PageFaultAccess) -> bool {
        match access {
            PageFaultAccess::Read => {
                area.map_perm.contains(MapPermission::R)
                    || area.map_perm.contains(MapPermission::W)
            }
            PageFaultAccess::Write => area.map_perm.contains(MapPermission::W),
            PageFaultAccess::Execute => area.map_perm.contains(MapPermission::X),
        }
    }

    fn prepare_fault_page(
        &self,
        vpn: VirtAddr,
        access: PageFaultAccess,
    ) -> Result<Option<PreparedFaultPage>, usize> {
        let areas = self.areas.read();
        let area = areas
            .iter()
            .find(|area| {
                area.vaddr_range.get_start() <= vpn && vpn < area.vaddr_range.get_end()
            })
            .ok_or(PAGE_FAULT_NO_AREA)?;
        if !Self::area_allows_access(area, access) {
            return Err(PAGE_FAULT_PERMISSION);
        }
        if area.data_frames.contains_key(vpn)
            || (self.shared_page_table
                && self
                    .page_table
                    .translate(vpn)
                    .is_some_and(|(_, flags)| flags.contains(MappingFlags::P)))
        {
            return Ok(None);
        }

        let file = area.file.clone();
        let file_offset = area.file_offset_at(vpn).ok_or(PAGE_FAULT_METADATA)?;
        let cacheable = !area.map_perm.contains(MapPermission::W);
        drop(areas);

        let (frame, copied) = if let Some(file) = file.as_deref() {
            load_file_page(file, file_offset, cacheable).ok_or(PAGE_FAULT_FRAME_ALLOC)?
        } else {
            (frame_alloc().ok_or(PAGE_FAULT_FRAME_ALLOC)?, 0)
        };
        Ok(Some(PreparedFaultPage {
            file,
            file_offset,
            cacheable,
            frame,
            copied,
        }))
    }

    pub fn from_existed_user(user_space: &MemorySet) -> Option<MemorySet> {
        trace!("os::mm::MemorySet::from_existed_user");
        let parent_page_table = user_space.page_table.clone();
        loop {
            // Pin a coherent-enough read snapshot, then construct the child
            // page table without excluding faults in the parent address space.
            // The write-side validation below turns it into an atomic snapshot.
            let snapshots: Vec<ForkAreaSnapshot> = {
                let parent_areas = user_space.areas.read();
                parent_areas.iter().map(ForkAreaSnapshot::capture).collect()
            };

            let resident_pages = snapshots
                .iter()
                .map(|snapshot| snapshot.frames.len())
                .sum();
            let mut child_mappings = Vec::with_capacity(resident_pages);
            let mut parent_updates = Vec::with_capacity(resident_pages);
            for snapshot in &snapshots {
                for (&vpn, frame) in &snapshot.frames {
                    let mut permission = snapshot.map_perm;
                    // Capturing the fork snapshot cloned every resident frame,
                    // so each private writable page is already shared with the
                    // child and must become copy-on-write.
                    if !snapshot.shared && permission.contains(MapPermission::W) {
                        permission.remove(MapPermission::W);
                    }
                    child_mappings.push((vpn, frame.paddr, permission.into()));
                    if !snapshot.shared && snapshot.map_perm.contains(MapPermission::W) {
                        parent_updates.push((vpn, frame.paddr, permission.into()));
                    }
                }
            }

            let mut memory_set = Self::new_bare();
            memory_set.page_table.map_pages_batch(&child_mappings);

            // The page-table lock verifies every physical-page identity while
            // applying the COW permissions. A concurrent VMA operation can
            // make this attempt stale, but cannot make it update a replacement
            // mapping accidentally.
            if !parent_page_table.update_permissions_batch(&parent_updates) {
                continue;
            }

            let program_brk = {
                let parent_areas = user_space.areas.exclusive_access();
                let unchanged = parent_areas.len() == snapshots.len()
                    && parent_areas
                        .iter()
                        .zip(snapshots.iter())
                        .all(|(area, snapshot)| snapshot.matches(area));
                if !unchanged {
                    drop(parent_areas);
                    continue;
                }
                user_space.program_brk()
            };

            let child_areas: Vec<_> = snapshots
                .into_iter()
                .map(ForkAreaSnapshot::into_map_area)
                .collect();
            *memory_set.areas.exclusive_access() = child_areas;
            memory_set.set_program_brk(program_brk);
            return Some(memory_set);
        }
    }

    pub fn prepare_shared_writable_pages(&mut self) -> bool {
        let page_table = self.page_table.clone();
        let mut areas = self.areas.exclusive_access();
        for area in areas.iter_mut() {
            if !area.map_perm.contains(MapPermission::W) {
                continue;
            }
            let vpns = area.data_frames.keys();
            for vpn in vpns {
                let mut data_frames = area.data_frames.lock_page(vpn);
                let Some(old) = data_frames.get(&vpn) else {
                    continue;
                };
                let Some((paddr, flags)) = page_table.translate(vpn) else {
                    continue;
                };
                if old.is_shared() && !flags.contains(MappingFlags::W) {
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
                    page_table.unmap_page(vpn);
                    page_table.map_page(vpn, f.paddr, area.map_perm.into(), MappingSize::Page4KB);
                    data_frames.insert(vpn, f);
                } else if !flags.contains(MappingFlags::W) {
                    page_table.unmap_page(vpn);
                    page_table.map_page(vpn, old.paddr, area.map_perm.into(), MappingSize::Page4KB);
                }
            }
        }
        true
    }

    pub fn handle_page_fault(
        &self,
        vaddr: polyhal::VirtAddr,
        access: PageFaultAccess,
    ) -> Option<PageFaultResult> {
        if vaddr.raw() < crate::config::PAGE_SIZE {
            return page_fault_failure(PAGE_FAULT_INVALID_ADDRESS);
        }
        let vpn = vaddr.floor();
        let mut prepared = match self.prepare_fault_page(vpn, access) {
            Ok(prepared) => prepared,
            Err(reason) => return page_fault_failure(reason),
        };
        let mut prepared_cow: Option<PreparedCowPage> = None;
        loop {
            let areas = self.areas.read();
            let area = match areas.iter().find(|area| {
                area.vaddr_range.get_start() <= vpn && vpn < area.vaddr_range.get_end()
            }) {
                Some(area) => area,
                None => return page_fault_failure(PAGE_FAULT_NO_AREA),
            };
            if !Self::area_allows_access(area, access) {
                return page_fault_failure(PAGE_FAULT_PERMISSION);
            }
            let mut data_frames = area.data_frames.lock_page(vpn);
            let write_fault = access == PageFaultAccess::Write;
            if let Some(cow) = prepared_cow.take() {
                if write_fault
                    && cow.still_matches(area, &data_frames, vpn, &self.page_table)
                {
                    let result = if cow.shared_translation {
                        PageFaultResult::SharedTranslationChanged
                    } else {
                        PageFaultResult::Handled
                    };
                    self.page_table.unmap_page(vpn);
                    self.page_table.map_page(
                        vpn,
                        cow.frame.paddr,
                        area.map_perm.into(),
                        MappingSize::Page4KB,
                    );
                    data_frames.insert(vpn, cow.frame);
                    return Some(result);
                }
            }
            if self.shared_page_table {
                if let Some((paddr, flags)) = self
                    .page_table
                    .translate(vpn)
                    .filter(|(_, flags)| flags.contains(MappingFlags::P))
                {
                    if write_fault && !flags.contains(MappingFlags::W) {
                        if let Some(source) = data_frames
                            .get(&vpn)
                            .filter(|frame| frame.paddr.raw() == paddr.raw())
                            .map(FrameTracker::shared_clone)
                        {
                            drop(data_frames);
                            drop(areas);
                            prepared_cow = match PreparedCowPage::new(source, true) {
                                Some(cow) => Some(cow),
                                None => return page_fault_failure(PAGE_FAULT_FRAME_ALLOC),
                            };
                            continue;
                        }
                        let f = match frame_alloc() {
                            Some(f) => f,
                            None => return page_fault_failure(PAGE_FAULT_FRAME_ALLOC),
                        };
                        unsafe {
                            core::ptr::copy_nonoverlapping(
                                paddr.get_ptr::<u8>(),
                                f.paddr.get_mut_ptr(),
                                PAGE_SIZE,
                            );
                        }
                        self.page_table.unmap_page(vpn);
                        self.page_table.map_page(
                            vpn,
                            f.paddr,
                            area.map_perm.into(),
                            MappingSize::Page4KB,
                        );
                        data_frames.insert(vpn, f);
                        return Some(PageFaultResult::SharedTranslationChanged);
                    }
                    self.page_table.unmap_page(vpn);
                    self.page_table.map_page(
                        vpn,
                        paddr.into(),
                        area.map_perm.into(),
                        MappingSize::Page4KB,
                    );
                    return Some(PageFaultResult::Handled);
                }
            }
            let in_frames = data_frames.contains_key(&vpn);
            if !in_frames {
                if !prepared
                    .as_ref()
                    .is_some_and(|prepared| prepared.still_matches(area, vpn))
                {
                    drop(data_frames);
                    drop(areas);
                    prepared = match self.prepare_fault_page(vpn, access) {
                        Ok(prepared) => prepared,
                        Err(reason) => return page_fault_failure(reason),
                    };
                    continue;
                }

                let PreparedFaultPage {
                    file: _file,
                    file_offset: _off,
                    frame: f,
                    copied,
                    ..
                } = prepared.take().expect("validated prepared fault page");
                debug_assert!(copied <= crate::config::PAGE_SIZE);
                #[cfg(feature = "smp-diagnostics")]
                let mut trace_rustc_driver_page = false;
                #[cfg(feature = "smp-diagnostics")]
                if let Some(file) = _file.as_deref() {
                    let buf = f
                        .paddr
                        .slice_with_len::<u8>(crate::config::PAGE_SIZE);
                    if area.map_perm.contains(MapPermission::X) && copied < buf.len() {
                        crate::println!(
                            "[mmap-diag] short executable page path={:?} off={:#x} copied={} page={} vpn={:#x}",
                            file.path(),
                            _off,
                            copied,
                            buf.len(),
                            vpn.raw(),
                        );
                    }
                    if file
                        .path()
                        .as_deref()
                        .is_some_and(|path| path.contains("librustc_driver"))
                        && _off == 0x7468_000
                    {
                        let head = unsafe { core::ptr::read_unaligned(buf.as_ptr() as *const u32) };
                        let instruction = unsafe {
                            core::ptr::read_unaligned(buf.as_ptr().add(0xe18) as *const u32)
                        };
                        crate::println!(
                            "[mmap-diag] driver-target-load off={:#x} copied={} head={:#010x} insn={:#010x} frame={:#x}",
                            _off,
                            copied,
                            head,
                            instruction,
                            f.paddr.raw(),
                        );
                        crate::mm::watch_frame(f.paddr, 0xe18, head, instruction);
                        trace_rustc_driver_page = true;
                    }
                }
                let permission = Self::pte_permission_for_frame(area, &f);
                self.page_table
                    .map_page(vpn, f.paddr, permission.into(), MappingSize::Page4KB);
                #[cfg(feature = "smp-diagnostics")]
                if trace_rustc_driver_page {
                    let head = unsafe { core::ptr::read_unaligned(f.paddr.get_ptr() as *const u32) };
                    let instruction = unsafe {
                        core::ptr::read_unaligned((f.paddr + 0xe18).get_ptr() as *const u32)
                    };
                    crate::println!(
                        "[mmap-diag] driver-target-post-map head={:#010x} insn={:#010x} frame={:#x}",
                        head,
                        instruction,
                        f.paddr.raw(),
                    );
                }
                data_frames.insert(vpn, f);
                return Some(if self.shared_page_table {
                    PageFaultResult::SharedMetadataChanged
                } else {
                    PageFaultResult::Handled
                });
            }
            if self.shared_page_table {
                let old = match data_frames.get(&vpn) {
                    Some(f) => f,
                    None => return page_fault_failure(PAGE_FAULT_METADATA),
                };
                self.page_table
                    .map_page(vpn, old.paddr, area.map_perm.into(), MappingSize::Page4KB);
                return Some(PageFaultResult::Handled);
            }
            let old = match data_frames.get(&vpn) {
                Some(f) => f,
                None => return page_fault_failure(PAGE_FAULT_METADATA),
            };
            let (paddr, flags) = match self.page_table.translate(vpn) {
                Some(x) => x,
                None => {
                    let perm = Self::pte_permission_for_frame(area, old);
                    self.page_table
                        .map_page(vpn, old.paddr, perm.into(), MappingSize::Page4KB);
                    return Some(PageFaultResult::Handled);
                }
            };
            if !flags.contains(MappingFlags::P) {
                let perm = Self::pte_permission_for_frame(area, old);
                self.page_table
                    .map_page(vpn, old.paddr, perm.into(), MappingSize::Page4KB);
                return Some(PageFaultResult::Handled);
            }
            if write_fault && !area.shared && old.is_shared() {
                if paddr.raw() != old.paddr.raw() {
                    return page_fault_failure(PAGE_FAULT_METADATA);
                }
                let source = old.shared_clone();
                drop(data_frames);
                drop(areas);
                prepared_cow = match PreparedCowPage::new(source, false) {
                    Some(cow) => Some(cow),
                    None => return page_fault_failure(PAGE_FAULT_FRAME_ALLOC),
                };
                continue;
            } else {
                let perm = Self::pte_permission_for_frame(area, old);
                self.page_table.unmap_page(vpn);
                self.page_table
                    .map_page(vpn, old.paddr, perm.into(), MappingSize::Page4KB);
            }
            return Some(PageFaultResult::Handled);
        }
    }

    pub fn from_shared(user_space: &MemorySet) -> MemorySet {
        MemorySet {
            page_table: user_space.page_table.clone(),
            areas: Arc::clone(&user_space.areas),
            shared_page_table: true,
            activity: Arc::clone(&user_space.activity),
            program_brk: Arc::clone(&user_space.program_brk),
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

    pub fn mapping_is_shared(&self, vaddr: VirtAddr) -> bool {
        let vpn = vaddr.floor();
        self.areas
            .read()
            .iter()
            .find(|area| {
                area.vaddr_range.get_start() <= vpn && vpn < area.vaddr_range.get_end()
            })
            .map(|area| area.shared)
            .unwrap_or(false)
    }

    #[cfg(any(
        feature = "smp-diagnostics",
        feature = "buildstorm-mm-diagnostics"
    ))]
    pub fn print_fault_mapping(&self, vaddr: VirtAddr) {
        let vpn = vaddr.floor();
        let areas = self.areas.read();
        let Some(area) = areas.iter().find(|area| {
            area.vaddr_range.get_start() <= vpn && vpn < area.vaddr_range.get_end()
        }) else {
            crate::println!("[mmap-diag] fault-map va={:#x} absent", vaddr.raw());
            return;
        };
        let pte = self.page_table.translate(vpn);
        let frame = area.data_frames.get_shared(vpn);
        crate::println!(
            "[mmap-diag] fault-map va={:#x} range={:#x}-{:#x} perm={:#x} shared={} file={:?} file_off={:#x} resident={} frame={:#x} pte={:#x} pte_flags={:#x}",
            vaddr.raw(),
            area.vaddr_range.get_start().raw(),
            area.vaddr_range.get_end().raw(),
            area.map_perm.bits(),
            area.shared,
            area.file.as_ref().and_then(|file| file.path()),
            area.file_offset,
            frame.is_some(),
            frame.as_ref().map(|frame| frame.paddr.raw()).unwrap_or(0),
            pte.map(|(paddr, _)| paddr.raw()).unwrap_or(0),
            pte.map(|(_, flags)| flags.bits()).unwrap_or(0),
        );
    }

    /// Print every VMA metadata entry that owns `paddr`.  It is used only
    /// after a watched physical page changes unexpectedly, to distinguish an
    /// allocator bug from an unintended writable alias in an address space.
    #[cfg(feature = "smp-diagnostics")]
    pub fn print_frame_aliases(&self, paddr: PhysAddr) {
        let areas = self.areas.read();
        for area in areas.iter() {
            let data_frames = area.data_frames.clone_shared_range(
                area.vaddr_range.get_start(),
                area.vaddr_range.get_end(),
            );
            for (vpn, frame) in data_frames {
                let pte = self.page_table.translate(vpn);
                let pte_paddr = pte.map(|(mapped, _)| mapped.raw()).unwrap_or(0);
                if frame.paddr.raw() == paddr.raw() || pte_paddr == paddr.raw() {
                    crate::println!(
                        "[frame-diag] alias va={:#x} range={:#x}-{:#x} perm={:#x} shared={} file={:?} meta={:#x} pte={:#x} pte_flags={:#x}",
                        vpn.raw(),
                        area.vaddr_range.get_start().raw(),
                        area.vaddr_range.get_end().raw(),
                        area.map_perm.bits(),
                        area.shared,
                        area.file.as_ref().and_then(|file| file.path()),
                        frame.paddr.raw(),
                        pte_paddr,
                        pte.map(|(_, flags)| flags.bits()).unwrap_or(0),
                    );
                }
            }
        }
    }

    /// Copy from an already validated user range without dereferencing a user
    /// virtual address in supervisor mode.
    pub fn copy_from_user(&self, addr: usize, dst: &mut [u8]) -> bool {
        if dst.is_empty() {
            return true;
        }
        let mut copied = 0usize;
        while copied < dst.len() {
            let Some(user_addr) = addr.checked_add(copied) else {
                return false;
            };
            let page_remaining = PAGE_SIZE - (user_addr & (PAGE_SIZE - 1));
            let chunk = page_remaining.min(dst.len() - copied);
            if !self.page_table.copy_from_user_page(
                VirtAddr::from(user_addr),
                &mut dst[copied..copied + chunk],
            ) {
                return false;
            }
            copied += chunk;
        }
        true
    }

    /// Copy to an already validated user range through the physical direct map.
    pub fn copy_to_user(&self, addr: usize, src: &[u8]) -> bool {
        if src.is_empty() {
            return true;
        }
        let mut copied = 0usize;
        while copied < src.len() {
            let Some(user_addr) = addr.checked_add(copied) else {
                return false;
            };
            let page_remaining = PAGE_SIZE - (user_addr & (PAGE_SIZE - 1));
            let chunk = page_remaining.min(src.len() - copied);
            if !self.page_table.copy_to_user_page(
                VirtAddr::from(user_addr),
                &src[copied..copied + chunk],
            ) {
                return false;
            }
            copied += chunk;
        }
        true
    }

    /// Copy to a prepared writable user range while keeping its VMA layout
    /// stable for the whole operation.
    ///
    /// Signal delivery needs this stronger variant: another thread sharing
    /// the address space may otherwise remove the VMA after the signal stack
    /// was faulted in but before the page-table copy starts.  Page-table COW
    /// or `madvise` can still invalidate a prepared PTE, in which case the
    /// caller must fault the range again and retry.
    pub(crate) fn copy_to_user_stable(&self, addr: usize, src: &[u8]) -> bool {
        if src.is_empty() {
            return true;
        }
        let Some(last) = addr.checked_add(src.len() - 1) else {
            return false;
        };
        let Some(end) = (last & !(PAGE_SIZE - 1)).checked_add(PAGE_SIZE) else {
            return false;
        };
        let mut cursor = addr & !(PAGE_SIZE - 1);
        let areas = self.areas.read();
        while cursor < end {
            let vaddr = VirtAddr::from(cursor);
            let Some(area) = areas.iter().find(|area| {
                area.vaddr_range.get_start() <= vaddr && vaddr < area.vaddr_range.get_end()
            }) else {
                return false;
            };
            if !area.map_perm.contains(MapPermission::U | MapPermission::W) {
                return false;
            }
            cursor = area.vaddr_range.get_end().raw().min(end);
        }

        let mut copied = 0usize;
        while copied < src.len() {
            let Some(user_addr) = addr.checked_add(copied) else {
                return false;
            };
            let page_remaining = PAGE_SIZE - (user_addr & (PAGE_SIZE - 1));
            let chunk = page_remaining.min(src.len() - copied);
            if !self.page_table.copy_to_user_page(
                VirtAddr::from(user_addr),
                &src[copied..copied + chunk],
            ) {
                return false;
            }
            copied += chunk;
        }
        true
    }

    /// Snapshot a readable user range while preventing concurrent VMA
    /// removal or permission changes.  This is the read-side counterpart of
    /// `copy_to_user_stable` and is used when restoring an rt signal frame.
    pub(crate) fn copy_from_user_stable(&self, addr: usize, dst: &mut [u8]) -> bool {
        if dst.is_empty() {
            return true;
        }
        let Some(last) = addr.checked_add(dst.len() - 1) else {
            return false;
        };
        let Some(end) = (last & !(PAGE_SIZE - 1)).checked_add(PAGE_SIZE) else {
            return false;
        };
        let mut cursor = addr & !(PAGE_SIZE - 1);
        let areas = self.areas.read();
        while cursor < end {
            let vaddr = VirtAddr::from(cursor);
            let Some(area) = areas.iter().find(|area| {
                area.vaddr_range.get_start() <= vaddr && vaddr < area.vaddr_range.get_end()
            }) else {
                return false;
            };
            if !area.map_perm.contains(MapPermission::U)
                || !area
                    .map_perm
                    .intersects(MapPermission::R | MapPermission::W)
            {
                return false;
            }
            cursor = area.vaddr_range.get_end().raw().min(end);
        }

        let mut copied = 0usize;
        while copied < dst.len() {
            let Some(user_addr) = addr.checked_add(copied) else {
                return false;
            };
            let page_remaining = PAGE_SIZE - (user_addr & (PAGE_SIZE - 1));
            let chunk = page_remaining.min(dst.len() - copied);
            if !self.page_table.copy_from_user_page(
                VirtAddr::from(user_addr),
                &mut dst[copied..copied + chunk],
            ) {
                return false;
            }
            copied += chunk;
        }
        true
    }

    pub fn clone_frame_metadata(&self, vpn: VirtAddr) -> Option<FrameTracker> {
        let areas = self.areas.read();
        let area = areas
            .iter()
            .find(|area| area.vaddr_range.get_start() <= vpn && vpn < area.vaddr_range.get_end())?;
        area.data_frames.get_shared(vpn)
    }
    pub fn replace_frame_metadata(&mut self, vpn: VirtAddr, frame: &FrameTracker) {
        if self.shared_page_table {
            return;
        }
        let areas = self.areas.read();
        if let Some(area) = areas.iter().find(|area| {
                area.vaddr_range.get_start() <= vpn && vpn < area.vaddr_range.get_end()
            })
        {
            area.data_frames
                .lock_page(vpn)
                .insert(vpn, frame.shared_clone());
        }
    }
    pub fn frame_metadata_range(
        &self,
        start: VirtAddr,
        end: VirtAddr,
    ) -> Vec<(VirtAddr, FrameTracker)> {
        let start = start.floor();
        let end = end.ceil();
        let mut frames = Vec::new();
        let areas = self.areas.read();
        for area in areas.iter() {
            if area.vaddr_range.get_start() >= end || area.vaddr_range.get_end() <= start {
                continue;
            }
            frames.extend(area.data_frames.clone_shared_range(start, end));
        }
        frames
    }

    pub fn area_metadata_range(&self, start: VirtAddr, end: VirtAddr) -> Vec<MapArea> {
        let start = start.floor();
        let end = end.ceil();
        let mut areas = Vec::new();
        let current = self.areas.read();
        for area in current.iter() {
            let area_start = area.vaddr_range.get_start();
            let area_end = area.vaddr_range.get_end();
            if area_start >= end || area_end <= start {
                continue;
            }
            let range_start = start.max(area_start);
            let range_end = end.min(area_end);
            let mut metadata = MapArea::from_another(area);
            let Some(file_offset) = area.file_offset_at(range_start) else {
                continue;
            };
            metadata.file_offset = file_offset;
            metadata.vaddr_range = VAddrRange::new(range_start, range_end);
            let selected_frames = area
                .data_frames
                .clone_shared_range(range_start, range_end);
            metadata.data_frames.replace(selected_frames);
            for &vpn in area.dirty.range(range_start..range_end) {
                metadata.dirty.insert(vpn);
            }
            areas.push(metadata);
        }
        areas
    }

    pub fn insert_shared_metadata_areas(&mut self, areas: &[MapArea]) {
        if self.shared_page_table {
            return;
        }
        for incoming in areas {
            let start = incoming.vaddr_range.get_start();
            let end = incoming.vaddr_range.get_end();
            self.munmap_metadata_only(start, end);
            let mut area = MapArea::from_another(incoming);
            let cloned_frames = incoming.data_frames.clone_shared_range(start, end);
            area.data_frames.replace(cloned_frames);
            area.dirty = incoming.dirty.clone();
            self.merge_or_push_metadata_area(area);
        }
        self.shared_page_table = true;
    }
    pub fn valid_user_buf(&self, ptr: *const u8, len: usize, need_write: bool) -> bool {
        if len == 0 {
            return true;
        }
        if ptr.is_null() {
            return false;
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
        let mut cursor = start;
        let areas = self.areas.read();
        while cursor < end {
            let vaddr = VirtAddr::from(cursor);
            let Some(area) = areas.iter().find(|area| {
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
            cursor = area.vaddr_range.get_end().raw().min(end);
        }
        true
    }

    pub fn user_range_mapped(&self, start: usize, len: usize) -> bool {
        if len == 0 {
            return true;
        }
        let Some(last) = start.checked_add(len.saturating_sub(1)) else {
            return false;
        };
        let mut cursor = start & !(crate::config::PAGE_SIZE - 1);
        let Some(end) =
            (last & !(crate::config::PAGE_SIZE - 1)).checked_add(crate::config::PAGE_SIZE)
        else {
            return false;
        };
        if end < cursor {
            return false;
        }
        let areas = self.areas.read();
        while cursor < end {
            let vaddr = VirtAddr::from(cursor);
            let Some(area) = areas.iter().find(|area| {
                area.vaddr_range.get_start() <= vaddr && vaddr < area.vaddr_range.get_end()
            }) else {
                return false;
            };
            if !area.map_perm.contains(MapPermission::U) {
                return false;
            }
            cursor = area.vaddr_range.get_end().raw().min(end);
        }
        true
    }
    fn recycle_data_pages_inner(&mut self, switch_current: bool) {
        let old_root = self.page_table.token().root().raw();
        if self.shared_page_table {
            if switch_current {
                // The scheduler still has this address space installed and
                // must first switch to BOOT_PAGE_TABLE and clear its active-CPU
                // bit. The reaper calls the inactive path afterwards.
                return;
            }
            // This TCB is leaving a CLONE_VM group.  The remaining members
            // still own the canonical areas and page table, so only detach
            // this local handle.
            self.page_table = retired_user_page_table();
            self.areas = Arc::new(VmRwLock::new_named(Vec::new(), "Vm.detached_areas"));
            self.activity = Arc::new(AddressSpaceActivity::new());
            self.shared_page_table = false;
            return;
        }
        if !switch_current {
            // The owning hart has already installed the boot page table.
            // Destroying the whole inactive address space does not require
            // thousands of individual PTE unmaps: PageTableWrapper::drop
            // releases all page-table levels and the retired MapAreas release
            // their FrameTrackers.  Replacing both owners first also keeps
            // those destructor paths out of VmLock.
            let old_page_table =
                core::mem::replace(&mut self.page_table, retired_user_page_table());
            let old_areas = core::mem::replace(
                &mut self.areas,
                Arc::new(VmRwLock::new_named(Vec::new(), "Vm.detached_areas")),
            );
            self.activity = Arc::new(AddressSpaceActivity::new());
            self.shared_page_table = false;
            drop(old_areas);
            drop(old_page_table);
            return;
        }
        let mut areas = self.areas.exclusive_access();
        let mut retired_areas = core::mem::take(&mut *areas);
        drop(areas);
        for area in retired_areas.iter_mut() {
            area.unmap(&self.page_table);
        }
        drop(retired_areas);
        let retired = retired_user_page_table();
        if switch_current && PageTable::current().root().raw() == old_root {
            retired.change();
        }
        self.page_table = retired;
        self.shared_page_table = false;
    }

    pub fn recycle_data_pages(&mut self) {
        self.recycle_data_pages_inner(true);
    }

    pub fn recycle_data_pages_inactive(&mut self) {
        self.recycle_data_pages_inner(false);
    }
    pub fn shrink_to(&mut self, limit_va: VirtAddr) {
        let retired_hint = self
            .areas
            .read()
            .iter()
            .filter(|area| area.vaddr_range.get_start() >= limit_va)
            .count();
        let mut retired_areas = Vec::with_capacity(retired_hint);
        let mut areas = self.areas.exclusive_access();
        let mut index = 0usize;
        while index < areas.len() {
            if areas[index].vaddr_range.get_start() >= limit_va {
                retired_areas.push(areas.remove(index));
            } else {
                index += 1;
            }
        }
        drop(areas);
        for area in retired_areas.iter_mut() {
            area.unmap(&self.page_table);
        }
        drop(retired_areas);
    }
    pub fn is_overlap(&self, start_addr: polyhal::VirtAddr, end_addr: polyhal::VirtAddr) -> bool {
        let areas = self.areas.read();
        for area in areas.iter() {
            if start_addr < area.vaddr_range.get_end() && end_addr > area.vaddr_range.get_start() {
                return true;
            }
        }
        false
    }

    pub fn find_free_area(&self, len: usize) -> polyhal::VirtAddr {
        let areas = self.areas.read();
        Self::find_free_area_in(&areas, len)
    }

    fn split_area_at(areas: &mut Vec<MapArea>, index: usize, split: VirtAddr) -> Result<usize, isize> {
        let area_start = areas[index].vaddr_range.get_start();
        let area_end = areas[index].vaddr_range.get_end();
        if split <= area_start || split >= area_end {
            return Ok(index);
        }

        let file_offset = areas[index].file_offset_at(split).ok_or(-12isize)?;
        let mut right = MapArea::from_another(&areas[index]);
        right.file_offset = file_offset;
        right.vaddr_range = VAddrRange::new(split, area_end);

        let moved_frames = areas[index].take_resident_frames(split, area_end);
        right.replace_resident_frames(moved_frames);
        let moved_dirty: Vec<VirtAddr> = areas[index]
            .dirty
            .range(split..area_end)
            .copied()
            .collect();
        for vpn in moved_dirty {
            areas[index].dirty.remove(&vpn);
            right.dirty.insert(vpn);
        }

        areas[index].vaddr_range = VAddrRange::new(area_start, split);
        areas.insert(index + 1, right);
        Ok(index + 1)
    }

    /// Resize an existing mapping while preserving all resident pages and
    /// leaving nonresident pages lazy.  The supported `MREMAP_MAYMOVE` path
    /// transfers the original frame trackers and recreates PTEs at the new
    /// address, rather than copying through a temporary user buffer.
    pub fn mremap(
        &mut self,
        old_start: VirtAddr,
        old_len: usize,
        new_len: usize,
        may_move: bool,
    ) -> Result<MremapResult, isize> {
        let old_end_raw = old_start.raw().checked_add(old_len).ok_or(-12isize)?;
        let new_end_at_old_raw = old_start.raw().checked_add(new_len).ok_or(-12isize)?;
        let old_end = VirtAddr::from(old_end_raw);
        let new_end_at_old = VirtAddr::from(new_end_at_old_raw);

        let mut areas = self.areas.exclusive_access();
        let mut source_index = areas
            .iter()
            .position(|area| {
                area.vaddr_range.get_start() <= old_start && old_end <= area.vaddr_range.get_end()
            })
            .ok_or(-14isize)?;

        if areas[source_index].file.is_some()
            && areas[source_index]
                .file_offset
                .checked_add(new_len)
                .is_none()
        {
            return Err(-12);
        }

        if old_start > areas[source_index].vaddr_range.get_start() {
            source_index = Self::split_area_at(&mut areas, source_index, old_start)?;
        }
        if old_end < areas[source_index].vaddr_range.get_end() {
            Self::split_area_at(&mut areas, source_index, old_end)?;
        }

        if new_len == old_len {
            return Ok(MremapResult::new(old_start.raw(), false, Vec::new()));
        }

        if new_len < old_len {
            let mut retired_frames = Vec::new();
            let area = &mut areas[source_index];
            let removed_frames = area.take_resident_frames(new_end_at_old, old_end);
            for &vpn in removed_frames.keys() {
                if self
                    .page_table
                    .translate(vpn)
                    .is_some_and(|(_, flags)| flags.contains(MappingFlags::P))
                {
                    self.page_table.unmap_page(vpn);
                }
                area.dirty.remove(&vpn);
            }
            retired_frames.extend(removed_frames.into_values());
            area.vaddr_range = VAddrRange::new(old_start, new_end_at_old);
            return Ok(MremapResult::new(old_start.raw(), true, retired_frames));
        }

        let can_grow_in_place = !areas.iter().enumerate().any(|(index, area)| {
            index != source_index
                && area.vaddr_range.get_start() < new_end_at_old
                && area.vaddr_range.get_end() > old_end
        });
        if can_grow_in_place {
            areas[source_index].vaddr_range = VAddrRange::new(old_start, new_end_at_old);
            return Ok(MremapResult::new(old_start.raw(), false, Vec::new()));
        }
        if !may_move {
            return Err(-12);
        }

        let target_start = Self::find_free_area_in(&areas, new_len);
        let target_end_raw = target_start.raw().checked_add(new_len).ok_or(-12isize)?;
        let target_end = VirtAddr::from(target_end_raw);
        let mut moved_area = areas.remove(source_index);
        let old_frames = moved_area.take_all_resident_frames();
        let old_dirty = core::mem::take(&mut moved_area.dirty);
        let mut new_frames = BTreeMap::new();
        let mut new_dirty = alloc::collections::BTreeSet::new();

        for (old_vpn, frame) in old_frames {
            let offset = old_vpn.raw() - old_start.raw();
            let new_vpn = VirtAddr::from(target_start.raw() + offset);
            if self
                .page_table
                .translate(old_vpn)
                .is_some_and(|(_, flags)| flags.contains(MappingFlags::P))
            {
                let permission = Self::pte_permission_for_frame(&moved_area, &frame);
                self.page_table.map_page(
                    new_vpn,
                    frame.paddr,
                    permission.into(),
                    MappingSize::Page4KB,
                );
                self.page_table.unmap_page(old_vpn);
            }
            new_frames.insert(new_vpn, frame);
        }
        for old_vpn in old_dirty {
            let offset = old_vpn.raw() - old_start.raw();
            new_dirty.insert(VirtAddr::from(target_start.raw() + offset));
        }

        moved_area.vaddr_range = VAddrRange::new(target_start, target_end);
        moved_area.replace_resident_frames(new_frames);
        moved_area.dirty = new_dirty;
        areas.push(moved_area);
        Ok(MremapResult::new(target_start.raw(), true, Vec::new()))
    }

    fn find_free_area_in(areas: &[MapArea], len: usize) -> polyhal::VirtAddr {
        let len = (len + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
        if areas.is_empty() {
            return polyhal::VirtAddr::from(PAGE_SIZE);
        }
        let mut bounds: alloc::vec::Vec<(usize, usize)> = areas
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
        self.areas.exclusive_access().push(map_area);
        true
    }

    pub fn insert_lazy_area(
        &mut self,
        start_addr: polyhal::VirtAddr,
        end_addr: polyhal::VirtAddr,
        permission: MapPermission,
    ) -> bool {
        let start_addr = start_addr.floor();
        let end_addr = end_addr.ceil();
        if start_addr >= end_addr {
            return false;
        }
        let mut areas = self.areas.exclusive_access();
        if Self::areas_overlap(&areas, start_addr, end_addr) {
            return false;
        }
        Self::merge_or_push_metadata_area_locked(
            &mut areas,
            MapArea::new(start_addr, end_addr, MapType::Framed, permission),
        );
        true
    }

    pub fn populate_lazy_range(
        &self,
        start_addr: polyhal::VirtAddr,
        end_addr: polyhal::VirtAddr,
        access: PageFaultAccess,
    ) -> bool {
        let mut page = start_addr.floor();
        let end = end_addr.ceil();
        while page < end {
            if self.handle_page_fault(page, access).is_none() {
                return false;
            }
            page = page + PAGE_SIZE;
        }
        true
    }

    /// Atomically choose the traditional high-water address for an anonymous
    /// mapping and publish its VMA.  CLONE_VM siblings have distinct task
    /// locks, so keeping these two actions under the shared VMA lock is
    /// required to prevent overlapping mappings.
    pub fn insert_lazy_area_auto(
        &mut self,
        len: usize,
        permission: MapPermission,
    ) -> Option<polyhal::VirtAddr> {
        let len = len.checked_add(PAGE_SIZE - 1)? & !(PAGE_SIZE - 1);
        if len == 0 {
            return None;
        }
        let mut areas = self.areas.exclusive_access();
        let max_end = areas
            .iter()
            .map(|area| area.vaddr_range.get_end().raw())
            .max()
            .unwrap_or(0);
        let start = max_end.checked_add(PAGE_SIZE - 1)? & !(PAGE_SIZE - 1);
        let end = start.checked_add(len)?;
        let start_addr = polyhal::VirtAddr::from(start);
        let end_addr = polyhal::VirtAddr::from(end);
        Self::merge_or_push_metadata_area_locked(
            &mut areas,
            MapArea::new(start_addr, end_addr, MapType::Framed, permission),
        );
        Some(start_addr)
    }

    fn areas_overlap(
        areas: &[MapArea],
        start_addr: polyhal::VirtAddr,
        end_addr: polyhal::VirtAddr,
    ) -> bool {
        areas.iter().any(|area| {
            start_addr < area.vaddr_range.get_end() && end_addr > area.vaddr_range.get_start()
        })
    }

    fn merge_or_push_metadata_area_locked(areas: &mut Vec<MapArea>, mut area: MapArea) {
        if !area.is_empty_metadata_mapping() {
            areas.push(area);
            return;
        }

        let mut start = area.vaddr_range.get_start();
        let mut end = area.vaddr_range.get_end();
        let mut i = 0;
        while i < areas.len() {
            if areas[i].can_merge_empty_metadata_mapping(&area) {
                let area_start = areas[i].vaddr_range.get_start();
                let area_end = areas[i].vaddr_range.get_end();
                if area_end == start || end == area_start {
                    start = start.min(area_start);
                    end = end.max(area_end);
                    areas.remove(i);
                    continue;
                }
            }
            i += 1;
        }
        area.vaddr_range = VAddrRange::new(start, end);
        areas.push(area);
    }

    fn merge_or_push_metadata_area(&mut self, area: MapArea) {
        let mut areas = self.areas.exclusive_access();
        Self::merge_or_push_metadata_area_locked(&mut areas, area);
    }

    pub fn insert_file_area(
        &mut self,
        start_addr: polyhal::VirtAddr,
        end_addr: polyhal::VirtAddr,
        permission: MapPermission,
        file: alloc::sync::Arc<dyn crate::fs::File>,
        offset: usize,
    ) -> bool {
        let start_addr = start_addr.floor();
        let end_addr = end_addr.ceil();
        if start_addr >= end_addr {
            return false;
        }
        let mut areas = self.areas.exclusive_access();
        if Self::areas_overlap(&areas, start_addr, end_addr) {
            return false;
        }
        let mut area = MapArea::new(start_addr, end_addr, MapType::Framed, permission);
        area.file = Some(file.clone());
        area.file_offset = offset;
        areas.push(area);
        true
    }

    /// The file-backed counterpart of `insert_lazy_area_auto`.  It shares the
    /// address selection lock with anonymous mappings, matching Linux's one
    /// address-space VMA transaction for a non-fixed mmap.
    pub fn insert_file_area_auto(
        &mut self,
        len: usize,
        permission: MapPermission,
        file: alloc::sync::Arc<dyn crate::fs::File>,
        offset: usize,
    ) -> Option<polyhal::VirtAddr> {
        let len = len.checked_add(PAGE_SIZE - 1)? & !(PAGE_SIZE - 1);
        if len == 0 {
            return None;
        }
        let mut areas = self.areas.exclusive_access();
        let max_end = areas
            .iter()
            .map(|area| area.vaddr_range.get_end().raw())
            .max()
            .unwrap_or(0);
        let start = max_end.checked_add(PAGE_SIZE - 1)? & !(PAGE_SIZE - 1);
        let end = start.checked_add(len)?;
        let start_addr = polyhal::VirtAddr::from(start);
        let end_addr = polyhal::VirtAddr::from(end);
        let mut area = MapArea::new(start_addr, end_addr, MapType::Framed, permission);
        area.file = Some(file);
        area.file_offset = offset;
        areas.push(area);
        Some(start_addr)
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
        let mut areas = self.areas.exclusive_access();
        let start_addr = if start_addr.raw() == 0 {
            Self::find_free_area_in(&areas, len)
        } else {
            start_addr
        };
        let end_addr = polyhal::VirtAddr::from(start_addr.raw() + len);
        if areas.iter().any(|area| {
            start_addr < area.vaddr_range.get_end() && end_addr > area.vaddr_range.get_start()
        }) {
            return None;
        }
        let mut area = MapArea::new(start_addr, end_addr, MapType::Framed, permission);
        area.shared = true;
        let mut resident_frames = BTreeMap::new();
        for (idx, vpn) in area.vaddr_range.clone().into_iter().enumerate() {
            let frame = frames[idx].shared_clone();
            self.page_table
                .map_page(vpn, frame.paddr, permission.into(), MappingSize::Page4KB);
            resident_frames.insert(vpn, frame);
        }
        area.replace_resident_frames(resident_frames);
        areas.push(area);
        Some(start_addr)
    }

    pub fn insert_shared_file_frames(
        &mut self,
        start_addr: polyhal::VirtAddr,
        len: usize,
        frames: &[FrameTracker],
        permission: MapPermission,
        file: alloc::sync::Arc<dyn crate::fs::File>,
        file_offset: usize,
    ) -> Option<polyhal::VirtAddr> {
        let addr = self.insert_shared_frames(start_addr, len, frames, permission)?;
        if let Some(area) = self
            .areas
            .exclusive_access()
            .iter_mut()
            .find(|area| area.vaddr_range.get_start() == addr)
        {
            area.file = Some(file);
            area.file_offset = file_offset;
        }
        Some(addr)
    }

    pub fn madvise_private_anonymous_dontneed(
        &mut self,
        start_addr: polyhal::VirtAddr,
        len: usize,
    ) -> (isize, Vec<FrameTracker>) {
        if len == 0 {
            return (0, Vec::new());
        }
        let end_addr = polyhal::VirtAddr::from(start_addr.raw().saturating_add(len)).ceil();
        let start_vpn = start_addr.floor();
        let resident_hint = {
            let areas = self.areas.read();
            areas
                .iter()
                .filter(|area| {
                    area.vaddr_range.get_start() < end_addr
                        && area.vaddr_range.get_end() > start_vpn
                })
                .map(|area| {
                    area.data_frames.count_range(
                        start_vpn.max(area.vaddr_range.get_start()),
                        end_addr.min(area.vaddr_range.get_end()),
                    )
                })
                .fold(0usize, usize::saturating_add)
        };
        let mut retired_entries = Vec::with_capacity(resident_hint);
        let mut areas = self.areas.exclusive_access();
        let mut cursor = start_vpn;
        while cursor < end_addr {
            let Some(area) = areas.iter().find(|area| {
                area.vaddr_range.get_start() <= cursor && cursor < area.vaddr_range.get_end()
            }) else {
                return (-12, Vec::new());
            };
            if area.file.is_some() || area.shared {
                return (-22, Vec::new());
            }
            cursor = area.vaddr_range.get_end().min(end_addr);
        }

        for area in areas.iter_mut() {
            let area_start = area.vaddr_range.get_start();
            let area_end = area.vaddr_range.get_end();
            if area_start >= end_addr || area_end <= start_vpn {
                continue;
            }
            let range_start = start_vpn.max(area_start);
            let range_end = end_addr.min(area_end);
            let removed_start = retired_entries.len();
            area.take_resident_frames_into(range_start, range_end, &mut retired_entries);
            for (vpn, frame) in &retired_entries[removed_start..] {
                #[cfg(feature = "smp-diagnostics")]
                if crate::mm::watched_frame_paddr()
                    .is_some_and(|watched| watched.raw() == frame.paddr.raw())
                {
                    crate::println!(
                        "[frame-diag] madvise-discard va={:#x} range={:#x}-{:#x} perm={:#x} shared={} file={:?}",
                        vpn.raw(),
                        area.vaddr_range.get_start().raw(),
                        area.vaddr_range.get_end().raw(),
                        area.map_perm.bits(),
                        area.shared,
                        area.file.as_ref().and_then(|file| file.path()),
                    );
                }
                area.dirty.remove(vpn);
            }
        }
        drop(areas);
        retired_entries.sort_unstable_by_key(|(vpn, _)| vpn.raw());
        let unmapped_pages: Vec<_> = retired_entries
            .iter()
            .map(|(vpn, frame)| (*vpn, frame.paddr))
            .collect();
        self.page_table
            .unmap_pages_if_match_batch(&unmapped_pages);
        let retired_frames = retired_entries
            .into_iter()
            .map(|(_, frame)| frame)
            .collect();
        (0, retired_frames)
    }

    pub fn msync(&mut self, start_addr: polyhal::VirtAddr, len: usize, flags: usize) -> isize {
        const MS_INVALIDATE: usize = 2;
        if start_addr.raw() % crate::config::PAGE_SIZE != 0 {
            return -22;
        }
        let end_addr = polyhal::VirtAddr::from(start_addr.raw().saturating_add(len));
        let mut areas = self.areas.exclusive_access();
        for area in areas.iter_mut() {
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
                Self::sync_shared_file_page(area, vpn);
                if flags & MS_INVALIDATE != 0 {
                    self.page_table.unmap_page(vpn);
                }
            }
        }
        0
    }

    pub fn mprotect(
        &mut self,
        start_addr: polyhal::VirtAddr,
        len: usize,
        permission: MapPermission,
    ) -> isize {
        if len == 0 {
            return 0;
        }
        let end_addr = polyhal::VirtAddr::from(start_addr.raw().saturating_add(len)).ceil();

        let mut areas = self.areas.exclusive_access();
        let mut cursor = start_addr;
        let mut unchanged = true;
        while cursor < end_addr {
            let Some(area) = areas.iter().find(|area| {
                area.vaddr_range.get_start() <= cursor && cursor < area.vaddr_range.get_end()
            }) else {
                return -12;
            };
            if permission.contains(MapPermission::W)
                && area.shared
                && area.file.as_ref().is_some_and(|file| !file.writable())
            {
                return -13;
            }
            unchanged &= area.map_perm == permission;
            cursor = area.vaddr_range.get_end().min(end_addr);
        }
        if unchanged {
            return 0;
        }

        let mut i = 0;
        while i < areas.len() {
            let area_start = areas[i].vaddr_range.get_start();
            let area_end = areas[i].vaddr_range.get_end();
            if area_start >= end_addr || area_end <= start_addr {
                i += 1;
                continue;
            }

            let protect_start = start_addr.max(area_start);
            let protect_end = end_addr.min(area_end);

            if protect_start > area_start {
                let Some(file_offset) = areas[i].file_offset_at(protect_start) else {
                    return -12;
                };
                let mut right = MapArea::from_another(&areas[i]);
                right.file_offset = file_offset;
                right.vaddr_range = VAddrRange::new(protect_start, area_end);
                let right_frames = areas[i].take_resident_frames(protect_start, area_end);
                right.replace_resident_frames(right_frames);
                let right_keys: Vec<VirtAddr> = areas[i]
                    .dirty
                    .range(protect_start..area_end)
                    .copied()
                    .collect();
                for key in right_keys {
                    if areas[i].dirty.remove(&key) {
                        right.dirty.insert(key);
                    }
                }
                areas[i].vaddr_range = VAddrRange::new(area_start, protect_start);
                areas.insert(i + 1, right);
                i += 1;
                continue;
            }

            if protect_end < area_end {
                let Some(file_offset) = areas[i].file_offset_at(protect_end) else {
                    return -12;
                };
                let mut right = MapArea::from_another(&areas[i]);
                right.file_offset = file_offset;
                right.vaddr_range = VAddrRange::new(protect_end, area_end);
                let right_frames = areas[i].take_resident_frames(protect_end, area_end);
                right.replace_resident_frames(right_frames);
                let right_keys: Vec<VirtAddr> = areas[i]
                    .dirty
                    .range(protect_end..area_end)
                    .copied()
                    .collect();
                for key in right_keys {
                    if areas[i].dirty.remove(&key) {
                        right.dirty.insert(key);
                    }
                }
                areas[i].vaddr_range = VAddrRange::new(area_start, protect_end);
                areas.insert(i + 1, right);
            }

            areas[i].set_perm(permission);
            i += 1;
        }
        drop(areas);

        // Once the metadata permission is published, concurrent faults use
        // the new permission automatically. Existing resident PTEs can be
        // collected under a shared VMA lock and changed under the page-table
        // lock without excluding unrelated faults and address-space readers.
        let mut pte_updates = Vec::new();
        let areas = self.areas.read();
        for area in areas.iter() {
            let area_start = area.vaddr_range.get_start();
            let area_end = area.vaddr_range.get_end();
            if area_start >= end_addr || area_end <= start_addr {
                continue;
            }
            let area_perm = area.map_perm;
            let area_shared = area.shared;
            area.data_frames.for_each(|vpn, frame| {
                let mut pte_perm = area_perm;
                if !area_shared
                    && pte_perm.contains(MapPermission::W)
                    && frame.is_shared()
                {
                    pte_perm.remove(MapPermission::W);
                }
                pte_updates.push((vpn, frame.paddr, pte_perm.into()));
            });
        }
        drop(areas);
        let _ = self.page_table.update_permissions_batch(&pte_updates);
        0
    }

    pub fn max_end_vaddr(&self) -> polyhal::VirtAddr {
        self.areas
            .read()
            .iter()
            .map(|a| a.vaddr_range.get_end())
            .max()
            .unwrap_or_else(|| polyhal::VirtAddr::new(0))
    }

    pub fn munmap(
        &mut self,
        start_addr: polyhal::VirtAddr,
        end_addr: polyhal::VirtAddr,
    ) -> UnmapResult {
        let start_vaddr = start_addr.floor();
        let end_vaddr = end_addr.ceil();

        let mut unmapped_any = false;
        let mut i = 0;
        let (resident_hint, retired_area_hint) = {
            let areas = self.areas.read();
            areas
                .iter()
                .filter(|area| {
                    area.vaddr_range.get_start() < end_vaddr
                        && area.vaddr_range.get_end() > start_vaddr
                })
                .fold((0usize, 0usize), |(frames, areas), area| {
                    (
                        frames.saturating_add(area.data_frames.count_range(
                            start_vaddr.max(area.vaddr_range.get_start()),
                            end_vaddr.min(area.vaddr_range.get_end()),
                        )),
                        areas.saturating_add(1),
                    )
                })
        };
        let mut retired_frames = Vec::with_capacity(resident_hint);
        let mut writebacks = Vec::with_capacity(resident_hint);
        let mut unmapped_pages = Vec::with_capacity(resident_hint);
        let mut retired_areas = Vec::with_capacity(retired_area_hint);
        let mut areas = self.areas.exclusive_access();

        while i < areas.len() {
            let area_start = areas[i].vaddr_range.get_start();
            let area_end = areas[i].vaddr_range.get_end();

            if area_start < end_vaddr && area_end > start_vaddr {
                let unmap_start = start_vaddr.max(area_start);
                let unmap_end = end_vaddr.min(area_end);

                let writeback_file = if areas[i].shared
                    && areas[i].map_perm.contains(MapPermission::W)
                {
                    areas[i]
                        .file
                        .as_ref()
                        .filter(|file| file.writable())
                        .cloned()
                } else {
                    None
                };
                let mapping_file_offset = areas[i].file_offset;
                let removed_frames = areas[i].take_resident_frames(unmap_start, unmap_end);
                for (vaddr, frame) in removed_frames {
                    unmapped_pages.push(vaddr);
                    areas[i].dirty.remove(&vaddr);
                    if let Some(file) = writeback_file.as_ref() {
                        if let Some(file_offset) = mapping_file_offset
                            .checked_add(vaddr.raw() - area_start.raw())
                        {
                            writebacks.push(DeferredSharedWriteback {
                                file: file.clone(),
                                file_offset,
                                frame,
                            });
                            continue;
                        }
                    }
                    retired_frames.push(frame);
                }
                let dirty_keys: alloc::vec::Vec<VirtAddr> = areas[i]
                    .dirty
                    .range(unmap_start..unmap_end)
                    .copied()
                    .collect();
                for vaddr in dirty_keys {
                    areas[i].dirty.remove(&vaddr);
                }
                unmapped_any = true;

                if unmap_start <= area_start && unmap_end >= area_end {
                    retired_areas.push(areas.remove(i));
                } else if unmap_start <= area_start {
                    let file_offset = areas[i]
                        .file_offset_at(unmap_end)
                        .expect("existing file mapping offset overflow");
                    areas[i].file_offset = file_offset;
                    areas[i].vaddr_range = VAddrRange::new(unmap_end, area_end);
                    i += 1;
                } else if unmap_end >= area_end {
                    areas[i].vaddr_range = VAddrRange::new(area_start, unmap_start);
                    i += 1;
                } else {
                    let file_offset = areas[i]
                        .file_offset_at(unmap_end)
                        .expect("existing file mapping offset overflow");
                    let mut right = MapArea::from_another(&areas[i]);
                    right.file_offset = file_offset;
                    right.vaddr_range = VAddrRange::new(unmap_end, area_end);
                    let right_frames = areas[i].take_resident_frames(unmap_end, area_end);
                    right.replace_resident_frames(right_frames);
                    areas[i].vaddr_range = VAddrRange::new(area_start, unmap_start);
                    areas.insert(i + 1, right);
                    i += 1;
                }
            } else {
                i += 1;
            }
        }

        drop(areas);
        self.page_table.unmap_pages_batch(&unmapped_pages);

        UnmapResult::new(unmapped_any, retired_frames, writebacks, retired_areas)
    }

    fn sync_shared_file_page(area: &MapArea, vpn: VirtAddr) {
        if !area.shared || !area.map_perm.contains(MapPermission::W) {
            return;
        }
        let Some(file) = area.file.as_ref() else {
            return;
        };
        if !file.writable() {
            return;
        }
        let Some(frame) = area.data_frames.get_shared(vpn) else {
            return;
        };
        let file_off = area.file_offset + (vpn.raw() - area.vaddr_range.get_start().raw());
        let file_size = file.stat().st_size.max(0) as usize;
        if file_off >= file_size {
            return;
        }
        let write_len = crate::config::PAGE_SIZE.min(file_size - file_off);
        let buf = frame.paddr.slice_mut_with_len(write_len);
        let _ = file.write_at(file_off, buf);
    }

    pub fn munmap_metadata_only(
        &mut self,
        start_addr: polyhal::VirtAddr,
        end_addr: polyhal::VirtAddr,
    ) -> bool {
        // CLONE_VM users share one canonical metadata vector.  The owning
        // operation has already changed it, so peer propagation must not
        // remove it a second time.
        if self.shared_page_table {
            return false;
        }
        let start_vaddr = start_addr.floor();
        let end_vaddr = end_addr.ceil();

        let mut unmapped_any = false;
        let mut i = 0;
        let (retired_frame_hint, retired_area_hint) = {
            let areas = self.areas.read();
            areas
                .iter()
                .filter(|area| {
                    area.vaddr_range.get_start() < end_vaddr
                        && area.vaddr_range.get_end() > start_vaddr
                })
                .fold((0usize, 0usize), |(frames, areas), area| {
                    (
                        frames.saturating_add(area.data_frames.count_range(
                            start_vaddr.max(area.vaddr_range.get_start()),
                            end_vaddr.min(area.vaddr_range.get_end()),
                        )),
                        areas.saturating_add(1),
                    )
                })
        };
        let mut retired_frames = Vec::with_capacity(retired_frame_hint);
        let mut retired_areas = Vec::with_capacity(retired_area_hint);
        let mut areas = self.areas.exclusive_access();

        while i < areas.len() {
            let area_start = areas[i].vaddr_range.get_start();
            let area_end = areas[i].vaddr_range.get_end();

            if area_start < end_vaddr && area_end > start_vaddr {
                let unmap_start = start_vaddr.max(area_start);
                let unmap_end = end_vaddr.min(area_end);

                let frame_keys = areas[i].data_frames.keys_range(unmap_start, unmap_end);
                let removed_frames = areas[i].take_resident_frames(unmap_start, unmap_end);
                retired_frames.extend(removed_frames.into_values());
                for vaddr in &frame_keys {
                    areas[i].dirty.remove(vaddr);
                }
                let dirty_keys: alloc::vec::Vec<VirtAddr> = areas[i]
                    .dirty
                    .range(unmap_start..unmap_end)
                    .copied()
                    .collect();
                for vaddr in dirty_keys {
                    areas[i].dirty.remove(&vaddr);
                }
                unmapped_any = true;

                if unmap_start <= area_start && unmap_end >= area_end {
                    retired_areas.push(areas.remove(i));
                } else if unmap_start <= area_start {
                    let file_offset = areas[i]
                        .file_offset_at(unmap_end)
                        .expect("existing file mapping offset overflow");
                    areas[i].file_offset = file_offset;
                    areas[i].vaddr_range = VAddrRange::new(unmap_end, area_end);
                    i += 1;
                } else if unmap_end >= area_end {
                    areas[i].vaddr_range = VAddrRange::new(area_start, unmap_start);
                    i += 1;
                } else {
                    let file_offset = areas[i]
                        .file_offset_at(unmap_end)
                        .expect("existing file mapping offset overflow");
                    let mut right = MapArea::from_another(&areas[i]);
                    right.file_offset = file_offset;
                    right.vaddr_range = VAddrRange::new(unmap_end, area_end);
                    let right_frames = areas[i].take_resident_frames(unmap_end, area_end);
                    right.replace_resident_frames(right_frames);
                    let right_dirty: alloc::vec::Vec<VirtAddr> =
                        areas[i].dirty.range(unmap_end..area_end).copied().collect();
                    for key in right_dirty {
                        areas[i].dirty.remove(&key);
                        right.dirty.insert(key);
                    }
                    areas[i].vaddr_range = VAddrRange::new(area_start, unmap_start);
                    areas.insert(i + 1, right);
                    i += 1;
                }
            } else {
                i += 1;
            }
        }

        drop(areas);
        drop(retired_frames);
        drop(retired_areas);
        unmapped_any
    }
}

const RESIDENT_FRAME_SHARDS: usize = 16;
type ResidentFrameMap = BTreeMap<VirtAddr, FrameTracker>;
static NEXT_RESIDENT_FRAME_SET_ID: AtomicUsize = AtomicUsize::new(1);

struct ResidentFrameGuard<'a> {
    inner: VmPagesGuard<'a, ResidentFrameMap>,
    generation: &'a AtomicUsize,
    count: &'a AtomicUsize,
    original_len: usize,
    mutated: bool,
}

impl Deref for ResidentFrameGuard<'_> {
    type Target = ResidentFrameMap;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl DerefMut for ResidentFrameGuard<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.mutated = true;
        &mut self.inner
    }
}

impl Drop for ResidentFrameGuard<'_> {
    fn drop(&mut self) {
        if self.mutated {
            let current_len = self.inner.len();
            if current_len > self.original_len {
                self.count
                    .fetch_add(current_len - self.original_len, Ordering::Release);
            } else if current_len < self.original_len {
                self.count
                    .fetch_sub(self.original_len - current_len, Ordering::Release);
            }
            self.generation.fetch_add(1, Ordering::Release);
        }
    }
}

struct ResidentFrames {
    // Keep the lock shards out of MapArea itself. MapArea values are moved and
    // copied through deep mmap/fork call chains on a 16 KiB kernel stack; an
    // inline array made each VMA nearly a kilobyte and overflowed that stack.
    shards: Box<[VmPagesLock<ResidentFrameMap>; RESIDENT_FRAME_SHARDS]>,
    id: usize,
    generation: AtomicUsize,
    count: AtomicUsize,
}

impl ResidentFrames {
    fn new() -> Self {
        Self {
            shards: Box::new(core::array::from_fn(|_| {
                VmPagesLock::new_named(BTreeMap::new(), "Vm.area_frames")
            })),
            id: NEXT_RESIDENT_FRAME_SET_ID.fetch_add(1, Ordering::Relaxed),
            generation: AtomicUsize::new(0),
            count: AtomicUsize::new(0),
        }
    }

    #[inline]
    fn id(&self) -> usize {
        self.id
    }

    #[inline]
    fn generation(&self) -> usize {
        self.generation.load(Ordering::Acquire)
    }

    #[inline]
    fn shard_index(vpn: VirtAddr) -> usize {
        (vpn.raw() >> 12) & (RESIDENT_FRAME_SHARDS - 1)
    }

    #[inline]
    fn lock_page(&self, vpn: VirtAddr) -> ResidentFrameGuard<'_> {
        let inner = self.shards[Self::shard_index(vpn)].lock();
        let original_len = inner.len();
        ResidentFrameGuard {
            inner,
            generation: &self.generation,
            count: &self.count,
            original_len,
            mutated: false,
        }
    }

    fn contains_key(&self, vpn: VirtAddr) -> bool {
        self.lock_page(vpn).contains_key(&vpn)
    }

    fn get_shared(&self, vpn: VirtAddr) -> Option<FrameTracker> {
        self.lock_page(vpn)
            .get(&vpn)
            .map(FrameTracker::shared_clone)
    }

    fn keys(&self) -> Vec<VirtAddr> {
        let mut keys = Vec::new();
        for shard in self.shards.iter() {
            keys.extend(shard.lock().keys().copied());
        }
        keys.sort_unstable();
        keys
    }

    fn keys_range(&self, start: VirtAddr, end: VirtAddr) -> Vec<VirtAddr> {
        let mut keys = Vec::new();
        for shard in self.shards.iter() {
            keys.extend(shard.lock().range(start..end).map(|(&vpn, _)| vpn));
        }
        keys.sort_unstable();
        keys
    }

    fn count_range(&self, start: VirtAddr, end: VirtAddr) -> usize {
        self.shards
            .iter()
            .map(|shard| shard.lock().range(start..end).count())
            .fold(0usize, usize::saturating_add)
    }

    fn clone_shared_range(
        &self,
        start: VirtAddr,
        end: VirtAddr,
    ) -> ResidentFrameMap {
        let mut frames = BTreeMap::new();
        for shard in self.shards.iter() {
            frames.extend(
                shard
                    .lock()
                    .range(start..end)
                    .map(|(&vpn, frame)| (vpn, frame.shared_clone())),
            );
        }
        frames
    }

    fn clone_shared_range_consistent(
        &self,
        start: VirtAddr,
        end: VirtAddr,
    ) -> (ResidentFrameMap, usize) {
        loop {
            let before = self.generation();
            let frames = self.clone_shared_range(start, end);
            let after = self.generation();
            if before == after {
                return (frames, after);
            }
        }
    }

    fn for_each(&self, mut visit: impl FnMut(VirtAddr, &FrameTracker)) {
        for shard in self.shards.iter() {
            let shard = shard.lock();
            for (&vpn, frame) in shard.iter() {
                visit(vpn, frame);
            }
        }
    }

    fn take_range(&self, start: VirtAddr, end: VirtAddr) -> ResidentFrameMap {
        let mut selected = BTreeMap::new();
        for shard in self.shards.iter() {
            let mut data_frames = shard.lock();
            let mut shard_selected = data_frames.split_off(&start);
            let mut tail = shard_selected.split_off(&end);
            data_frames.append(&mut tail);
            selected.append(&mut shard_selected);
        }
        self.count.fetch_sub(selected.len(), Ordering::Release);
        self.generation.fetch_add(1, Ordering::Release);
        selected
    }

    fn take_range_into(
        &self,
        start: VirtAddr,
        end: VirtAddr,
        selected: &mut Vec<(VirtAddr, FrameTracker)>,
    ) {
        let original_len = selected.len();
        for shard in self.shards.iter() {
            let mut data_frames = shard.lock();
            let mut shard_selected = data_frames.split_off(&start);
            let mut tail = shard_selected.split_off(&end);
            data_frames.append(&mut tail);
            drop(data_frames);
            selected.extend(shard_selected);
        }
        let removed = selected.len() - original_len;
        self.count.fetch_sub(removed, Ordering::Release);
        self.generation.fetch_add(1, Ordering::Release);
    }

    fn take_all(&self) -> ResidentFrameMap {
        let mut frames = BTreeMap::new();
        for shard in self.shards.iter() {
            frames.append(&mut core::mem::take(&mut *shard.lock()));
        }
        self.count.store(0, Ordering::Release);
        self.generation.fetch_add(1, Ordering::Release);
        frames
    }

    fn replace(&self, frames: ResidentFrameMap) {
        let frame_count = frames.len();
        let mut partitioned: [ResidentFrameMap; RESIDENT_FRAME_SHARDS] =
            core::array::from_fn(|_| BTreeMap::new());
        for (vpn, frame) in frames {
            partitioned[Self::shard_index(vpn)].insert(vpn, frame);
        }
        for (shard, frames) in self.shards.iter().zip(partitioned) {
            *shard.lock() = frames;
        }
        self.count.store(frame_count, Ordering::Release);
        self.generation.fetch_add(1, Ordering::Release);
    }

    fn is_empty(&self) -> bool {
        self.count.load(Ordering::Acquire) == 0
    }

    fn clear(&self) {
        drop(self.take_all());
    }
}

impl Drop for ResidentFrames {
    fn drop(&mut self) {
        let frame_count = self.count.load(Ordering::Acquire);
        if frame_count == 0 {
            return;
        }
        let mut retired = Vec::new();
        if retired.try_reserve_exact(frame_count).is_err() {
            return;
        }
        for shard in self.shards.iter() {
            let frames = core::mem::take(&mut *shard.lock());
            retired.extend(frames.into_values());
        }
        self.count.store(0, Ordering::Release);
        frames_dealloc_batch(retired);
    }
}

pub struct MapArea {
    pub vaddr_range: VAddrRange,
    data_frames: ResidentFrames,
    map_type: MapType,
    map_perm: MapPermission,
    file: Option<alloc::sync::Arc<dyn crate::fs::File>>,
    file_offset: usize,
    shared: bool,
    dirty: alloc::collections::BTreeSet<VirtAddr>,
}

impl MapArea {
    fn take_resident_frames(
        &self,
        start: VirtAddr,
        end: VirtAddr,
    ) -> BTreeMap<VirtAddr, FrameTracker> {
        self.data_frames.take_range(start, end)
    }

    fn take_resident_frames_into(
        &self,
        start: VirtAddr,
        end: VirtAddr,
        selected: &mut Vec<(VirtAddr, FrameTracker)>,
    ) {
        self.data_frames.take_range_into(start, end, selected);
    }

    fn take_all_resident_frames(&self) -> BTreeMap<VirtAddr, FrameTracker> {
        self.data_frames.take_all()
    }

    fn replace_resident_frames(&self, frames: BTreeMap<VirtAddr, FrameTracker>) {
        self.data_frames.replace(frames);
    }

    fn file_offset_at(&self, start: VirtAddr) -> Option<usize> {
        if self.file.is_none() {
            return Some(self.file_offset);
        }
        self.file_offset
            .checked_add(start.raw().checked_sub(self.vaddr_range.get_start().raw())?)
    }

    pub fn set_perm(&mut self, perm: MapPermission) {
        self.map_perm = perm;
    }

    fn is_empty_metadata_mapping(&self) -> bool {
        self.file.is_none() && self.data_frames.is_empty() && self.dirty.is_empty()
    }

    fn can_merge_empty_metadata_mapping(&self, other: &MapArea) -> bool {
        self.is_empty_metadata_mapping()
            && other.is_empty_metadata_mapping()
            && self.map_type == other.map_type
            && self.map_perm == other.map_perm
            && self.shared == other.shared
            && self.file_offset == other.file_offset
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
            data_frames: ResidentFrames::new(),
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
            data_frames: ResidentFrames::new(),
            map_type: another.map_type,
            map_perm: another.map_perm,
            file: another.file.clone(),
            file_offset: another.file_offset,
            shared: another.shared,
            dirty: alloc::collections::BTreeSet::new(),
        }
    }
    pub fn clone_shared_metadata(another: &MapArea) -> Self {
        Self {
            vaddr_range: VAddrRange::new(
                another.vaddr_range.get_start(),
                another.vaddr_range.get_end(),
            ),
            data_frames: {
                let frames = ResidentFrames::new();
                frames.replace(another.data_frames.clone_shared_range(
                    another.vaddr_range.get_start(),
                    another.vaddr_range.get_end(),
                ));
                frames
            },
            map_type: another.map_type,
            map_perm: another.map_perm,
            file: another.file.clone(),
            file_offset: another.file_offset,
            shared: another.shared,
            dirty: another.dirty.clone(),
        }
    }
    fn map(&mut self, page_table: &Arc<AddressSpacePageTable>) -> Result<(), ()> {
        trace!("os::mm::memory_set::MapArea::map");
        let mut data_frames = BTreeMap::new();
        for vaddr in self.vaddr_range {
            let p_tracker = match frame_alloc() {
                Some(frame) => frame,
                None => {
                    self.unmap(page_table);
                    return Err(());
                }
            };
            page_table.map_page(
                vaddr,
                p_tracker.paddr,
                self.map_perm.into(),
                MappingSize::Page4KB,
            );
            data_frames.insert(vaddr, p_tracker);
        }
        self.data_frames.replace(data_frames);
        Ok(())
    }

    /// Unmap page area
    #[allow(unused)]
    fn unmap(&self, page_table: &Arc<AddressSpacePageTable>) {
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
    fn copy_data(
        &mut self,
        _page_table: &Arc<AddressSpacePageTable>,
        data: &[u8],
        page_off: usize,
    ) {
        trace!("os::mm::memory_set::MapArea::copy_data");
        assert_eq!(self.map_type, MapType::Framed);
        let mut data_pos: usize = 0;
        let mut curr_vaddr = self.vaddr_range.get_start();
        let mut page_off = page_off;
        let len = data.len();
        while data_pos < len {
            let chunk = (PAGE_SIZE - page_off).min(len - data_pos);
            let src = &data[data_pos..data_pos + chunk];
            let data_frames = self.data_frames.lock_page(curr_vaddr);
            let frame = data_frames.get(&curr_vaddr).unwrap();
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
            let paddr = {
                let data_frames = self.data_frames.lock_page(curr_vaddr);
                let Some(frame) = data_frames.get(&curr_vaddr) else {
                    return false;
                };
                frame.paddr
            };
            // This MapArea is still private to the exec loader, so the frame
            // cannot disappear. Never keep the IRQ-off resident-frame lock
            // across filesystem I/O.
            let dst = &mut (paddr + page_off).slice_mut_with_len(chunk);
            if !read_exact_at(file, file_offset, dst) {
                return false;
            }
            #[cfg(feature = "smp-diagnostics")]
            if curr_vaddr.raw() == 0x15_8000 {
                let mut retry = alloc::vec![0u8; chunk];
                let retry_ok = read_exact_at(file, file_offset, &mut retry);
                let dst_info = unsafe {
                    core::ptr::read_unaligned((paddr + page_off + 0x404).get_ptr() as *const u32)
                };
                let retry_info = u32::from_le_bytes(retry[0x404..0x408].try_into().unwrap());
                crate::println!(
                    "[elf-diag] page-read off={:#x} len={} retry_ok={} dst={:#010x} retry={:#010x}",
                    file_offset,
                    chunk,
                    retry_ok,
                    dst_info,
                    retry_info,
                );
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
        if self.contains(MapPermission::R) || self.contains(MapPermission::W) {
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
