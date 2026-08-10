cfg_if::cfg_if! {
    if #[cfg(target_arch = "loongarch64")] {
        mod loongarch64;
        pub use loongarch64::*;
    } else if #[cfg(target_arch = "aarch64")] {
        mod aarch64;
        pub use aarch64::*;
    } else if #[cfg(target_arch = "riscv64")] {
        mod riscv64;
        pub use riscv64::*;

    } else if #[cfg(target_arch = "x86_64")] {
        mod x86_64;
        pub use x86_64::*;
    } else {
        compile_error!("unsupported architecture!");
    }
}

use core::ops::Deref;

use crate::{components::common::frame_alloc, PhysAddr, VirtAddr};

use super::common::frame_dealloc;

/// The size of the page table.
pub const PAGE_SIZE: usize = PageTable::PAGE_SIZE;

/// Page table entry structure
///
/// Just define here. Should implement functions in specific architectures.
#[derive(Copy, Clone, Debug)]
pub struct PTE(pub usize);

impl PTE {
    pub const fn empty() -> Self {
        Self(0)
    }
}

/// Page Table
///
/// This is just the page table defination.
/// The implementation of the page table in the specific architecture mod.
/// Such as:
/// x86_64/page_table.rs
/// riscv64/page_table/sv39.rs
/// aarch64/page_table.rs
/// loongarch64/page_table.rs
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct PageTable(PhysAddr);

impl PageTable {
    /// Get the root Physical Page
    pub const fn root(&self) -> PhysAddr {
        self.0
    }
    /// Get the page table list through the physical address
    #[inline]
    pub(crate) fn get_pte_list(paddr: PhysAddr) -> &'static mut [PTE] {
        paddr.slice_mut_with_len::<PTE>(Self::PTE_NUM_IN_PAGE)
    }

    /// Mapping a page to specific virtual page (user space address).
    ///
    /// Ensure that PageTable is which you want to map.
    /// vpn: Virtual page will be mapped.
    /// ppn: Physical page.
    /// flags: Mapping flags, include Read, Write, Execute and so on.
    /// size: MappingSize. Just support 4KB page currently.
    pub fn map_page(
        &self,
        vaddr: VirtAddr,
        paddr: PhysAddr,
        flags: MappingFlags,
        _size: MappingSize,
    ) {
        let mut pte_list = Self::get_pte_list(self.0);
        if Self::PAGE_LEVEL == 4 {
            let pte = &mut pte_list[vaddr.pn_index(3)];
            if !pte.is_valid() {
                *pte = PTE::new_table(frame_alloc());
            }
            pte_list = Self::get_pte_list(pte.address());
        }
        // level 3
        {
            let pte = &mut pte_list[vaddr.pn_index(2)];
            if !pte.is_valid() {
                *pte = PTE::new_table(frame_alloc());
            }
            pte_list = Self::get_pte_list(pte.address());
        }
        // level 2
        {
            let pte = &mut pte_list[vaddr.pn_index(1)];
            if !pte.is_valid() {
                *pte = PTE::new_table(frame_alloc());
            }
            pte_list = Self::get_pte_list(pte.address());
        }
        // level 1, map page
        pte_list[vaddr.pn_index(0)] = PTE::new_page(paddr, flags.into());
        TLB::flush_vaddr(vaddr);
    }

    /// Map a set of base pages and perform a single TLB flush afterwards.
    pub fn map_pages_batch(&self, mappings: &[(VirtAddr, PhysAddr, MappingFlags)]) {
        for &(vaddr, paddr, flags) in mappings {
            let mut pte_list = Self::get_pte_list(self.0);
            if Self::PAGE_LEVEL == 4 {
                let pte = &mut pte_list[vaddr.pn_index(3)];
                if !pte.is_valid() {
                    *pte = PTE::new_table(frame_alloc());
                }
                pte_list = Self::get_pte_list(pte.address());
            }
            let pte = &mut pte_list[vaddr.pn_index(2)];
            if !pte.is_valid() {
                *pte = PTE::new_table(frame_alloc());
            }
            pte_list = Self::get_pte_list(pte.address());

            let pte = &mut pte_list[vaddr.pn_index(1)];
            if !pte.is_valid() {
                *pte = PTE::new_table(frame_alloc());
            }
            pte_list = Self::get_pte_list(pte.address());
            pte_list[vaddr.pn_index(0)] = PTE::new_page(paddr, flags.into());
        }
        if !mappings.is_empty() {
            TLB::flush_all();
        }
    }

    /// Mapping a page to specific address(kernel space address).
    ///
    /// TODO: This method is not implemented.
    /// TIPS: If we mapped to kernel, the page will be shared between different pagetable.
    ///
    /// Ensure that PageTable is which you want to map.
    /// vpn: Virtual page will be mapped.
    /// ppn: Physical page.
    /// flags: Mapping flags, include Read, Write, Execute and so on.
    /// size: MappingSize. Just support 4KB page currently.    
    ///
    /// How to implement shared.
    pub fn map_kernel(
        &self,
        vaddr: VirtAddr,
        paddr: PhysAddr,
        flags: MappingFlags,
        _size: MappingSize,
    ) {
        let mut pte_list = Self::get_pte_list(self.0);
        if Self::PAGE_LEVEL == 4 {
            let pte = &mut pte_list[vaddr.pn_index(3)];
            if !pte.is_valid() {
                *pte = PTE::new_table(frame_alloc());
            }
            pte_list = Self::get_pte_list(pte.address());
        }
        // level 3
        {
            let pte = &mut pte_list[vaddr.pn_index(2)];
            if !pte.is_valid() {
                *pte = PTE::new_table(frame_alloc());
            }
            pte_list = Self::get_pte_list(pte.address());
        }
        // level 2
        {
            let pte = &mut pte_list[vaddr.pn_index(1)];
            if !pte.is_valid() {
                *pte = PTE::new_table(frame_alloc());
            }
            pte_list = Self::get_pte_list(pte.address());
        }
        // level 1, map page
        pte_list[vaddr.pn_index(0)] = PTE::new_page(paddr, flags.into());
        TLB::flush_vaddr(vaddr);
    }

    /// Unmap a page from specific virtual page (user space address).
    ///
    /// Ensure the virtual page is exists.
    /// vpn: Virtual address.
    pub fn unmap_page(&self, vaddr: VirtAddr) {
        let mut pte_list = Self::get_pte_list(self.0);
        if Self::PAGE_LEVEL == 4 {
            let pte = &mut pte_list[vaddr.pn_index(3)];
            if !pte.is_table() {
                return;
            };
            pte_list = Self::get_pte_list(pte.address());
        }
        // level 3
        {
            let pte = &mut pte_list[vaddr.pn_index(2)];
            if !pte.is_table() {
                return;
            };
            pte_list = Self::get_pte_list(pte.address());
        }
        // level 2
        {
            let pte = &mut pte_list[vaddr.pn_index(1)];
            if !pte.is_table() {
                return;
            };
            pte_list = Self::get_pte_list(pte.address());
        }
        // level 1, map page
        pte_list[vaddr.pn_index(0)] = PTE(0);
        TLB::flush_vaddr(vaddr);
    }

    /// Unmap base pages with one page-table walk and one final TLB flush.
    pub fn unmap_pages_batch(&self, pages: &[VirtAddr]) {
        let mut changed = false;
        let leaf_span = Self::PAGE_SIZE * Self::PTE_NUM_IN_PAGE;
        let mut cached_leaf_key = usize::MAX;
        let mut cached_leaf_table = None;
        for &vaddr in pages {
            let leaf_key = vaddr.raw() / leaf_span;
            if leaf_key != cached_leaf_key {
                cached_leaf_key = leaf_key;
                let mut pte_list = Self::get_pte_list(self.0);
                let mut valid = true;
                if Self::PAGE_LEVEL == 4 {
                    let pte = &mut pte_list[vaddr.pn_index(3)];
                    if !pte.is_table() {
                        valid = false;
                    } else {
                        pte_list = Self::get_pte_list(pte.address());
                    }
                }
                if valid {
                    let pte = &mut pte_list[vaddr.pn_index(2)];
                    if !pte.is_table() {
                        valid = false;
                    } else {
                        pte_list = Self::get_pte_list(pte.address());
                    }
                }
                if valid {
                    let pte = &mut pte_list[vaddr.pn_index(1)];
                    cached_leaf_table = if pte.is_table() {
                        Some(pte.address())
                    } else {
                        None
                    };
                } else {
                    cached_leaf_table = None;
                }
            }
            let Some(leaf_table) = cached_leaf_table else {
                continue;
            };
            let pte = &mut Self::get_pte_list(leaf_table)[vaddr.pn_index(0)];
            if pte.is_valid() {
                *pte = PTE(0);
                changed = true;
            }
        }
        if changed {
            TLB::flush_all();
        }
    }

    /// Unmap base pages only while they still point at the expected frames.
    /// Callers can use this after publishing a metadata change without
    /// clearing a replacement mapping installed by a concurrent page fault.
    pub fn unmap_pages_if_match_batch(&self, pages: &[(VirtAddr, PhysAddr)]) {
        let mut changed = false;
        let leaf_span = Self::PAGE_SIZE * Self::PTE_NUM_IN_PAGE;
        let mut cached_leaf_key = usize::MAX;
        let mut cached_leaf_table = None;
        for &(vaddr, expected_paddr) in pages {
            let leaf_key = vaddr.raw() / leaf_span;
            if leaf_key != cached_leaf_key {
                cached_leaf_key = leaf_key;
                let mut pte_list = Self::get_pte_list(self.0);
                let mut valid = true;
                if Self::PAGE_LEVEL == 4 {
                    let pte = &mut pte_list[vaddr.pn_index(3)];
                    if !pte.is_table() {
                        valid = false;
                    } else {
                        pte_list = Self::get_pte_list(pte.address());
                    }
                }
                if valid {
                    let pte = &mut pte_list[vaddr.pn_index(2)];
                    if !pte.is_table() {
                        valid = false;
                    } else {
                        pte_list = Self::get_pte_list(pte.address());
                    }
                }
                if valid {
                    let pte = &mut pte_list[vaddr.pn_index(1)];
                    cached_leaf_table = if pte.is_table() {
                        Some(pte.address())
                    } else {
                        None
                    };
                } else {
                    cached_leaf_table = None;
                }
            }
            let Some(leaf_table) = cached_leaf_table else {
                continue;
            };
            let pte = &mut Self::get_pte_list(leaf_table)[vaddr.pn_index(0)];
            if pte.is_valid() && pte.address().raw() == expected_paddr.raw() {
                *pte = PTE(0);
                changed = true;
            }
        }
        if changed {
            TLB::flush_all();
        }
    }

    /// Replace leaf permissions only when the physical mapping still matches.
    pub fn update_pages_flags_batch(
        &self,
        updates: &[(VirtAddr, PhysAddr, MappingFlags)],
    ) -> bool {
        let mut complete = true;
        let mut changed = false;
        for &(vaddr, expected_paddr, flags) in updates {
            let mut pte_list = Self::get_pte_list(self.0);
            if Self::PAGE_LEVEL == 4 {
                let pte = &mut pte_list[vaddr.pn_index(3)];
                if !pte.is_table() {
                    complete = false;
                    continue;
                }
                pte_list = Self::get_pte_list(pte.address());
            }
            let pte = &mut pte_list[vaddr.pn_index(2)];
            if !pte.is_table() {
                complete = false;
                continue;
            }
            pte_list = Self::get_pte_list(pte.address());

            let pte = &mut pte_list[vaddr.pn_index(1)];
            if !pte.is_table() {
                complete = false;
                continue;
            }
            pte_list = Self::get_pte_list(pte.address());
            let pte = &mut pte_list[vaddr.pn_index(0)];
            if !pte.is_valid() || pte.address().raw() != expected_paddr.raw() {
                complete = false;
                continue;
            }
            *pte = PTE::new_page(expected_paddr, flags.into());
            changed = true;
        }
        if changed {
            TLB::flush_all();
        }
        complete
    }

    /// Translate a virtual adress to a physical address and mapping flags.
    ///
    /// Return None if the vaddr isn't mapped.
    /// vpn: The virtual address will be translated.
    pub fn translate(&self, vaddr: VirtAddr) -> Option<(PhysAddr, MappingFlags)> {
        let mut pte_list = Self::get_pte_list(self.0);
        if Self::PAGE_LEVEL == 4 {
            let pte = &mut pte_list[vaddr.pn_index(3)];
            if !pte.is_table() {
                return None;
            }
            pte_list = Self::get_pte_list(pte.address());
        }
        // level 3
        {
            let pte = &mut pte_list[vaddr.pn_index(2)];
            if !pte.is_table() {
                return None;
            }
            pte_list = Self::get_pte_list(pte.address());
        }
        // level 2
        {
            let pte = &mut pte_list[vaddr.pn_index(1)];
            if !pte.is_table() {
                return None;
            }
            pte_list = Self::get_pte_list(pte.address());
        }
        // level 1, map page
        let pte = pte_list[vaddr.pn_index(0)];
        if !pte.is_valid() {
            return None;
        }
        Some((
            PhysAddr::new(pte.address().raw() + vaddr.pn_offest(0)),
            pte.flags().into(),
        ))
    }

    /// Release the page table entry.
    ///
    /// The page table entry in the user space address will be released.
    /// [Page Table Wikipedia](https://en.wikipedia.org/wiki/Page_table).
    /// You don't need to care about this if you just want to use.
    pub fn release(&self) {
        let drop_l2 = |pte_list: &[PTE]| {
            pte_list.iter().for_each(|x| {
                if x.is_table() {
                    frame_dealloc(x.address());
                }
            });
        };
        let drop_l3 = |pte_list: &[PTE]| {
            pte_list.iter().for_each(|x| {
                if x.is_table() {
                    drop_l2(Self::get_pte_list(x.address()));
                    frame_dealloc(x.address());
                }
            });
        };
        let drop_l4 = |pte_list: &[PTE]| {
            pte_list.iter().for_each(|x| {
                if x.is_table() {
                    drop_l3(Self::get_pte_list(x.address()));
                    frame_dealloc(x.address());
                }
            });
        };

        // Drop all sub page table entry and clear root page.
        let pte_list = &mut Self::get_pte_list(self.0)[..Self::GLOBAL_ROOT_PTE_RANGE];
        if Self::PAGE_LEVEL == 4 {
            drop_l4(pte_list);
        } else {
            drop_l3(pte_list);
        }
        pte_list.fill(PTE(0));
    }
}

bitflags::bitflags! {
    /// Mapping flags for page table.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub struct MappingFlags: u64 {
        /// Persent
        const P = bit!(0);
        /// User Accessable Flag
        const U = bit!(1);
        /// Readable Flag
        const R = bit!(2);
        /// Writeable Flag
        const W = bit!(3);
        /// Executeable Flag
        const X = bit!(4);
        /// Accessed Flag
        const A = bit!(5);
        /// Dirty Flag, indicating that the page was written
        const D = bit!(6);
        /// Global Flag
        const G = bit!(7);
        /// Device Flag, indicating that the page was used for device memory
        const Device = bit!(8);
        /// Cache Flag, indicating that the page will be cached
        const Cache = bit!(9);

        /// Read | Write | Executeable Flags
        const RWX = Self::R.bits() | Self::W.bits() | Self::X.bits();
        /// User | Read | Write Flags
        const URW = Self::U.bits() | Self::R.bits() | Self::W.bits();
        /// User | Read | Executeable Flags
        const URX = Self::U.bits() | Self::R.bits() | Self::X.bits();
        /// User | Read | Write | Executeable Flags
        const URWX = Self::URW.bits() | Self::X.bits();
    }
}

/// This structure indicates size of the page that will be mapped.
///
/// TODO: Support More Page Size, 16KB or 32KB
/// Just support 4KB right now.
#[derive(Debug)]
pub enum MappingSize {
    Page4KB,
    // Page2MB,
    // Page1GB,
}

/// TLB Operation set.
/// Such as flush_vaddr, flush_all.
/// Just use it in the fn.
///
/// there are some methods in the TLB implementation
///
/// ### Flush the tlb entry through the specific virtual address
///
/// ```rust
/// TLB::flush_vaddr(arg0);  arg0 should be VirtAddr
/// ```
/// ### Flush all tlb entries
/// ```rust
/// TLB::flush_all();
/// ```
pub struct TLB;

/// Page Table Wrapper
///
/// You can use this wrapper to packing PageTable.
/// If you release the PageTableWrapper,
/// the PageTable will release its page table entry.
#[derive(Debug)]
pub struct PageTableWrapper(pub PageTable);

impl Deref for PageTableWrapper {
    type Target = PageTable;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

/// Allocate a new PageTableWrapper with new page table root
///
/// This operation will restore the page table.
impl PageTableWrapper {
    /// Alloc a new PageTableWrapper with new page table root
    /// This operation will copy kernel page table space from booting page table.
    #[inline]
    pub fn alloc() -> Self {
        let pt = PageTable(frame_alloc());
        pt.restore();
        Self(pt)
    }
}

/// Page Table Release.
///
/// You must implement this trait to release page table.
/// Include the page table entry and root page.
impl Drop for PageTableWrapper {
    fn drop(&mut self) {
        self.0.release();
        frame_dealloc(self.0 .0);
    }
}

impl VirtAddr {
    /// Get n level page table index of the given virtual address
    #[inline]
    pub fn pn_index(&self, n: usize) -> usize {
        (self.raw() >> (12 + 9 * n)) & 0x1ff
    }
    /// Get n level page table offset of the given virtual address
    #[inline]
    pub fn pn_offest(&self, n: usize) -> usize {
        self.raw() % (1 << (12 + 9 * n))
    }
}
