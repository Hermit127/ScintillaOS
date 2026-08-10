use super::{frame_alloc, frames_alloc_batch, FrameTracker};
use crate::config::PAGE_SIZE;
use crate::fs::{File, FilePageCacheKey};
use crate::sync::PageCacheRwLock;
use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU8, AtomicUsize, Ordering};

const FILE_PAGE_CACHE_MAX_PAGES: usize = 131_072;
const FILE_PAGE_CACHE_SHARDS: usize = 64;
pub(crate) const FILE_PAGE_READAHEAD_SIZE: usize = 16 * PAGE_SIZE;
const FILE_PAGE_READAHEAD_MAX_SIZE: usize = 128 * PAGE_SIZE;
const FILE_PAGE_LOAD_PENDING: u8 = 0;
const FILE_PAGE_LOAD_COMPLETE: u8 = 1;
const FILE_PAGE_LOAD_INVALIDATED: u8 = 2;

struct InFlightFilePageLoad {
    generation: usize,
    end_offset: usize,
    owner: usize,
    state: Arc<AtomicU8>,
}

#[derive(Clone, Copy)]
struct FileReadaheadState {
    next_offset: usize,
    window_size: usize,
}

struct FilePageCacheState {
    generations: BTreeMap<FilePageCacheKey, usize>,
    file_sizes: BTreeMap<FilePageCacheKey, usize>,
    pages: BTreeMap<FilePageCacheKey, BTreeMap<usize, FrameTracker>>,
    in_flight: BTreeMap<(FilePageCacheKey, usize), InFlightFilePageLoad>,
    readahead: BTreeMap<FilePageCacheKey, FileReadaheadState>,
    page_count: usize,
}

impl FilePageCacheState {
    const fn new() -> Self {
        Self {
            generations: BTreeMap::new(),
            file_sizes: BTreeMap::new(),
            pages: BTreeMap::new(),
            in_flight: BTreeMap::new(),
            readahead: BTreeMap::new(),
            page_count: 0,
        }
    }
}

static FILE_PAGE_CACHES: [PageCacheRwLock<FilePageCacheState>; FILE_PAGE_CACHE_SHARDS] = [const {
    PageCacheRwLock::new_named(FilePageCacheState::new(), "Vm.file_page_cache")
}; FILE_PAGE_CACHE_SHARDS];
static FILE_PAGE_CACHE_PAGE_COUNT: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "lock-stats")]
static FILE_PAGE_CACHE_READ_HITS: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "lock-stats")]
static FILE_PAGE_CACHE_READ_MISSES: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "lock-stats")]
static FILE_PAGE_CACHE_HIT_BYTES: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "lock-stats")]
static FILE_PAGE_CACHE_MISS_BYTES: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "lock-stats")]
static FILE_PAGE_CACHE_FILL_BYTES: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "lock-stats")]
static FILE_PAGE_CACHE_INVALIDATIONS: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "lock-stats")]
static FILE_PAGE_CACHE_INVALIDATED_PAGES: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "lock-stats")]
static FILE_PAGE_CACHE_RANGE_INVALIDATIONS: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "lock-stats")]
static FILE_PAGE_CACHE_RANGE_INVALIDATED_PAGES: AtomicUsize = AtomicUsize::new(0);

#[cfg(feature = "lock-stats")]
pub(crate) fn record_file_page_cache_hit(bytes: usize) {
    FILE_PAGE_CACHE_READ_HITS.fetch_add(1, Ordering::Relaxed);
    FILE_PAGE_CACHE_HIT_BYTES.fetch_add(bytes, Ordering::Relaxed);
}

#[cfg(feature = "lock-stats")]
pub(crate) fn record_file_page_cache_miss(bytes: usize) {
    FILE_PAGE_CACHE_READ_MISSES.fetch_add(1, Ordering::Relaxed);
    FILE_PAGE_CACHE_MISS_BYTES.fetch_add(bytes, Ordering::Relaxed);
}

#[cfg(feature = "lock-stats")]
pub fn dump_file_page_cache_stats() {
    crate::println!(
        "[file-page-cache-stats] pages={} read_hits={} read_misses={} hit_bytes={} miss_bytes={} fill_bytes={} invalidations={} invalidated_pages={} range_invalidations={} range_invalidated_pages={}",
        FILE_PAGE_CACHE_PAGE_COUNT.load(Ordering::Relaxed),
        FILE_PAGE_CACHE_READ_HITS.load(Ordering::Relaxed),
        FILE_PAGE_CACHE_READ_MISSES.load(Ordering::Relaxed),
        FILE_PAGE_CACHE_HIT_BYTES.load(Ordering::Relaxed),
        FILE_PAGE_CACHE_MISS_BYTES.load(Ordering::Relaxed),
        FILE_PAGE_CACHE_FILL_BYTES.load(Ordering::Relaxed),
        FILE_PAGE_CACHE_INVALIDATIONS.load(Ordering::Relaxed),
        FILE_PAGE_CACHE_INVALIDATED_PAGES.load(Ordering::Relaxed),
        FILE_PAGE_CACHE_RANGE_INVALIDATIONS.load(Ordering::Relaxed),
        FILE_PAGE_CACHE_RANGE_INVALIDATED_PAGES.load(Ordering::Relaxed),
    );
}

fn file_page_cache(key: FilePageCacheKey) -> &'static PageCacheRwLock<FilePageCacheState> {
    let hash = key.super_block.rotate_right(13)
        ^ (key.inode as usize).wrapping_mul(0x9e37_79b1usize);
    &FILE_PAGE_CACHES[hash & (FILE_PAGE_CACHE_SHARDS - 1)]
}

fn reserve_file_page_cache_slot() -> bool {
    let mut count = FILE_PAGE_CACHE_PAGE_COUNT.load(Ordering::Acquire);
    loop {
        if count >= FILE_PAGE_CACHE_MAX_PAGES {
            return false;
        }
        match FILE_PAGE_CACHE_PAGE_COUNT.compare_exchange_weak(
            count,
            count + 1,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => return true,
            Err(current) => count = current,
        }
    }
}

fn release_file_page_cache_slots(count: usize) {
    if count != 0 {
        let previous = FILE_PAGE_CACHE_PAGE_COUNT.fetch_sub(count, Ordering::AcqRel);
        debug_assert!(previous >= count);
    }
}

pub(crate) enum FilePageCacheRead {
    Hit(usize),
    Miss(usize),
}

pub(crate) struct FilePageCacheWriteGuard {
    key: Option<FilePageCacheKey>,
}

enum FilePageLoadLookup {
    Hit(FrameTracker),
    Load(FilePageLoadGuard),
    Wait(Arc<AtomicU8>),
}

pub(crate) enum FilePageCacheReadLoad {
    Retry,
    Bypass(usize),
    Load(FilePageLoadGuard),
    Wait(Arc<AtomicU8>),
}

pub(crate) struct FilePageLoadGuard {
    key: FilePageCacheKey,
    chunk_offset: usize,
    read_len: usize,
    generation: usize,
    state: Arc<AtomicU8>,
    active: bool,
}

impl FilePageLoadGuard {
    pub(crate) fn generation(&self) -> usize {
        self.generation
    }

    fn finish(&mut self) {
        if self.active {
            finish_file_page_load(self.key, self.chunk_offset, &self.state);
            self.active = false;
        }
    }
}

impl Drop for FilePageLoadGuard {
    fn drop(&mut self) {
        self.finish();
    }
}

fn current_file_page_load_owner() -> usize {
    crate::task::current_task()
        .map(|task| Arc::as_ptr(&task) as usize)
        .unwrap_or(0)
}

impl FilePageCacheWriteGuard {
    pub(crate) fn new(key: Option<FilePageCacheKey>) -> Self {
        if let Some(key) = key {
            invalidate_file_page_cache(key);
        }
        Self { key }
    }
}

impl Drop for FilePageCacheWriteGuard {
    fn drop(&mut self) {
        if let Some(key) = self.key {
            invalidate_file_page_cache(key);
        }
    }
}

fn lookup_file_page(key: FilePageCacheKey, offset: usize) -> Result<FrameTracker, usize> {
    let cache = file_page_cache(key).read();
    if let Some(frame) = cache.pages.get(&key).and_then(|pages| pages.get(&offset)) {
        return Ok(frame.shared_clone());
    }
    Err(cache.generations.get(&key).copied().unwrap_or(0))
}

fn lookup_or_reserve_file_page_load(key: FilePageCacheKey, offset: usize) -> FilePageLoadLookup {
    let owner = current_file_page_load_owner();
    {
        let cache = file_page_cache(key).read();
        if let Some(frame) = cache.pages.get(&key).and_then(|pages| pages.get(&offset)) {
            return FilePageLoadLookup::Hit(frame.shared_clone());
        }
    }
    let mut cache = file_page_cache(key).write();
    if let Some(frame) = cache.pages.get(&key).and_then(|pages| pages.get(&offset)) {
        return FilePageLoadLookup::Hit(frame.shared_clone());
    }

    let generation = *cache.generations.entry(key).or_insert(0);
    if let Some(load) = cache.in_flight.iter().find_map(|((load_key, start), load)| {
        (*load_key == key
            && load.generation == generation
            && offset >= *start
            && offset < load.end_offset
            && load.state.load(Ordering::Acquire) == FILE_PAGE_LOAD_PENDING)
            .then_some(load)
    }) {
        return FilePageLoadLookup::Wait(load.state.clone());
    }

    let (chunk_offset, read_len) = match cache.readahead.get(&key).copied() {
        Some(previous) if previous.next_offset == offset => (
            offset,
            previous
                .window_size
                .saturating_mul(2)
                .min(FILE_PAGE_READAHEAD_MAX_SIZE),
        ),
        _ => (
            offset & !(FILE_PAGE_READAHEAD_SIZE - 1),
            FILE_PAGE_READAHEAD_SIZE,
        ),
    };
    cache.readahead.insert(
        key,
        FileReadaheadState {
            next_offset: chunk_offset.saturating_add(read_len),
            window_size: read_len,
        },
    );
    let load_key = (key, chunk_offset);
    if let Some(load) = cache.in_flight.get(&load_key) {
        if load.generation == generation
            && load.state.load(Ordering::Acquire) == FILE_PAGE_LOAD_PENDING
        {
            return FilePageLoadLookup::Wait(load.state.clone());
        }
    }

    let stale_state = cache.in_flight.remove(&load_key).map(|stale| stale.state);
    if let Some(state) = stale_state.as_ref() {
        state.store(FILE_PAGE_LOAD_INVALIDATED, Ordering::Release);
    }
    let state = Arc::new(AtomicU8::new(FILE_PAGE_LOAD_PENDING));
    cache.in_flight.insert(
        load_key,
        InFlightFilePageLoad {
            generation,
            end_offset: chunk_offset.saturating_add(read_len),
            owner,
            state: state.clone(),
        },
    );
    let result = FilePageLoadLookup::Load(FilePageLoadGuard {
        key,
        chunk_offset,
        read_len,
        generation,
        state,
        active: true,
    });
    drop(cache);
    if let Some(state) = stale_state {
        crate::task::wake_file_page_load_waiters(Arc::as_ptr(&state) as usize);
    }
    result
}

fn cached_file_read_available(
    cache: &FilePageCacheState,
    key: FilePageCacheKey,
    offset: usize,
    request_len: usize,
) -> bool {
    if request_len == 0 {
        return true;
    }
    let Some(file_size) = cache.file_sizes.get(&key).copied() else {
        return false;
    };
    if offset >= file_size {
        return true;
    }
    let read_len = request_len.min(file_size - offset);
    let Some(end) = offset.checked_add(read_len) else {
        return false;
    };
    let Some(pages) = cache.pages.get(&key) else {
        return false;
    };
    let mut page_offset = offset & !(PAGE_SIZE - 1);
    while page_offset < end {
        if !pages.contains_key(&page_offset) {
            return false;
        }
        page_offset = page_offset.saturating_add(PAGE_SIZE);
    }
    true
}

pub(crate) fn reserve_file_page_cache_read(
    key: FilePageCacheKey,
    offset: usize,
    request_len: usize,
) -> FilePageCacheReadLoad {
    let owner = current_file_page_load_owner();
    let (chunk_offset, read_len) = if request_len < FILE_PAGE_READAHEAD_SIZE {
        let aligned = offset & !(FILE_PAGE_READAHEAD_SIZE - 1);
        let within_window = offset - aligned;
        let chunk_offset = if request_len <= FILE_PAGE_READAHEAD_SIZE - within_window {
            aligned
        } else {
            offset
        };
        (chunk_offset, FILE_PAGE_READAHEAD_SIZE)
    } else {
        (offset, request_len)
    };

    let mut cache = file_page_cache(key).write();
    if cached_file_read_available(&cache, key, offset, request_len) {
        return FilePageCacheReadLoad::Retry;
    }
    let generation = *cache.generations.entry(key).or_insert(0);
    if let Some(load) = cache.in_flight.iter().find_map(|((load_key, start), load)| {
        (*load_key == key
            && load.generation == generation
            && offset >= *start
            && offset < load.end_offset
            && load.state.load(Ordering::Acquire) == FILE_PAGE_LOAD_PENDING)
            .then_some(load)
    }) {
        if owner != 0 && load.owner == owner {
            return FilePageCacheReadLoad::Bypass(generation);
        }
        return FilePageCacheReadLoad::Wait(load.state.clone());
    }

    let load_key = (key, chunk_offset);
    let stale_state = cache.in_flight.remove(&load_key).map(|stale| stale.state);
    if let Some(state) = stale_state.as_ref() {
        state.store(FILE_PAGE_LOAD_INVALIDATED, Ordering::Release);
    }
    let state = Arc::new(AtomicU8::new(FILE_PAGE_LOAD_PENDING));
    cache.in_flight.insert(
        load_key,
        InFlightFilePageLoad {
            generation,
            end_offset: chunk_offset.saturating_add(read_len),
            owner,
            state: state.clone(),
        },
    );
    let load = FilePageLoadGuard {
        key,
        chunk_offset,
        read_len,
        generation,
        state,
        active: true,
    };
    drop(cache);
    if let Some(state) = stale_state {
        crate::task::wake_file_page_load_waiters(Arc::as_ptr(&state) as usize);
    }
    FilePageCacheReadLoad::Load(load)
}

fn finish_file_page_load(key: FilePageCacheKey, chunk_offset: usize, state: &Arc<AtomicU8>) {
    state.store(FILE_PAGE_LOAD_COMPLETE, Ordering::Release);
    crate::task::wake_file_page_load_waiters(Arc::as_ptr(state) as usize);
    let mut cache = file_page_cache(key).write();
    let load_key = (key, chunk_offset);
    let remove = cache
        .in_flight
        .get(&load_key)
        .is_some_and(|load| Arc::ptr_eq(&load.state, state));
    if remove {
        cache.in_flight.remove(&load_key);
    }
}

pub(crate) fn wait_for_file_page_load(state: &Arc<AtomicU8>) {
    let mut backoff = 1usize;
    while state.load(Ordering::Acquire) == FILE_PAGE_LOAD_PENDING {
        if crate::task::current_task().is_some() {
            let load_id = Arc::as_ptr(state) as usize;
            crate::task::block_current_and_run_next_if(
                crate::task::BlockReason::FilePageLoad(load_id),
                || state.load(Ordering::Acquire) == FILE_PAGE_LOAD_PENDING,
            );
        } else {
            for _ in 0..backoff {
                core::hint::spin_loop();
            }
            backoff = (backoff << 1).min(64);
        }
    }
}

fn publish_file_page(
    key: FilePageCacheKey,
    offset: usize,
    generation: usize,
    loaded: FrameTracker,
) -> FrameTracker {
    let mut cache = file_page_cache(key).write();
    if cache.generations.get(&key).copied().unwrap_or(0) != generation {
        return loaded;
    }
    if let Some(frame) = cache.pages.get(&key).and_then(|pages| pages.get(&offset)) {
        let cached = frame.shared_clone();
        drop(cache);
        drop(loaded);
        return cached;
    }

    let reserved_slot = reserve_file_page_cache_slot();
    let oldest = if !reserved_slot {
        cache.pages.iter().next().and_then(|(&oldest_key, pages)| {
            pages
                .keys()
                .next()
                .copied()
                .map(|offset| (oldest_key, offset))
        })
    } else {
        None
    };
    if !reserved_slot && oldest.is_none() {
        return loaded;
    }
    let mut retired = None;
    let mut retired_empty_map = None;
    if let Some((oldest_key, oldest_offset)) = oldest {
        let remove_empty_map = {
            let pages = cache
                .pages
                .get_mut(&oldest_key)
                .expect("oldest file cache remains present while locked");
            retired = pages.remove(&oldest_offset);
            pages.is_empty()
        };
        if retired.is_some() {
            cache.page_count -= 1;
        }
        if remove_empty_map {
            retired_empty_map = cache.pages.remove(&oldest_key);
        }
    }

    let replaced = cache
        .pages
        .entry(key)
        .or_default()
        .insert(offset, loaded.shared_clone());
    debug_assert!(replaced.is_none());
    cache.page_count += 1;
    drop(cache);
    drop(retired);
    drop(retired_empty_map);
    drop(replaced);
    loaded
}

fn publish_file_read_pages(
    key: FilePageCacheKey,
    generation: usize,
    file_size: usize,
    loaded: Vec<(usize, FrameTracker)>,
) {
    // Reserve retirement storage before entering the IRQ-off cache section.
    // Each incoming page can replace at most one duplicate and evict at most
    // one old page.
    let mut retired_frames = Vec::with_capacity(loaded.len().saturating_mul(2));
    let mut retired_maps = Vec::with_capacity(loaded.len());
    let mut cache = file_page_cache(key).write();
    if cache.generations.get(&key).copied().unwrap_or(0) != generation {
        drop(cache);
        drop(loaded);
        return;
    }
    cache.file_sizes.insert(key, file_size);

    for (offset, frame) in loaded {
        if cache
            .pages
            .get(&key)
            .is_some_and(|pages| pages.contains_key(&offset))
        {
            retired_frames.push(frame);
            continue;
        }

        let reserved_slot = reserve_file_page_cache_slot();
        if !reserved_slot {
            let oldest = cache.pages.iter().find_map(|(&oldest_key, pages)| {
                pages
                    .keys()
                    .next()
                    .copied()
                    .map(|oldest_offset| (oldest_key, oldest_offset))
            });
            if let Some((oldest_key, oldest_offset)) = oldest {
                let (retired, remove_empty_map) = {
                    let pages = cache
                        .pages
                        .get_mut(&oldest_key)
                        .expect("oldest file cache remains present while locked");
                    let retired = pages.remove(&oldest_offset);
                    (retired, pages.is_empty())
                };
                if let Some(retired) = retired {
                    cache.page_count -= 1;
                    retired_frames.push(retired);
                }
                if remove_empty_map {
                    if let Some(retired_map) = cache.pages.remove(&oldest_key) {
                        retired_maps.push(retired_map);
                    }
                }
            } else {
                retired_frames.push(frame);
                continue;
            }
        }

        let replaced = cache.pages.entry(key).or_default().insert(offset, frame);
        if let Some(replaced) = replaced {
            retired_frames.push(replaced);
        } else {
            cache.page_count += 1;
        }
    }

    drop(cache);
    drop(retired_frames);
    drop(retired_maps);
}

pub(crate) fn load_file_page(
    file: &dyn File,
    offset: usize,
    cacheable: bool,
) -> Option<(FrameTracker, usize)> {
    // Writable MAP_PRIVATE segments remain process-private. Sharing their
    // initial frame requires a fully atomic COW transition across fork and
    // CLONE_VM; keeping only immutable file pages in this cache preserves the
    // useful text/rodata sharing without allowing libc data-page aliases.
    if let Some(key) = cacheable.then(|| file.page_cache_key()).flatten() {
        loop {
            match lookup_or_reserve_file_page_load(key, offset) {
                FilePageLoadLookup::Hit(frame) => return Some((frame, PAGE_SIZE)),
                FilePageLoadLookup::Wait(state) => {
                    wait_for_file_page_load(&state);
                }
                FilePageLoadLookup::Load(mut load) => {
                    let mut readahead = Vec::new();
                    if readahead.try_reserve_exact(load.read_len).is_ok() {
                        readahead.resize(load.read_len, 0);
                        let copied =
                            file.read_at(load.chunk_offset, &mut readahead).max(0) as usize;
                        let copied = copied.min(readahead.len());
                        let source_offset = offset - load.chunk_offset;
                        let page_copied = copied.saturating_sub(source_offset).min(PAGE_SIZE);

                        // OSInode publishes every complete page loaded by the
                        // bulk read before returning from read_at().
                        if let Ok(frame) = lookup_file_page(key, offset) {
                            load.finish();
                            return Some((frame, page_copied));
                        }

                        let frame = frame_alloc()?;
                        let page = frame.paddr.slice_mut_with_len(PAGE_SIZE);
                        if page_copied != 0 {
                            page[..page_copied].copy_from_slice(
                                &readahead[source_offset..source_offset + page_copied],
                            );
                        }
                        let frame = publish_file_page(key, offset, load.generation, frame);
                        load.finish();
                        return Some((frame, page_copied));
                    }

                    let frame = frame_alloc()?;
                    let page = frame.paddr.slice_mut_with_len(PAGE_SIZE);
                    let copied = file.read_at(offset, page).max(0) as usize;
                    let copied = copied.min(page.len());
                    if copied < page.len() {
                        page[copied..].fill(0);
                    }
                    let frame = publish_file_page(key, offset, load.generation, frame);
                    load.finish();
                    return Some((frame, copied));
                }
            }
        }
    }

    let frame = frame_alloc()?;
    let page = frame.paddr.slice_mut_with_len(PAGE_SIZE);
    let copied = file.read_at(offset, page).max(0) as usize;
    let copied = copied.min(page.len());
    if copied < page.len() {
        page[copied..].fill(0);
    }

    Some((frame, copied))
}

pub(crate) fn read_file_page_cache(
    key: FilePageCacheKey,
    offset: usize,
    buf: &mut [u8],
) -> FilePageCacheRead {
    if buf.is_empty() {
        return FilePageCacheRead::Hit(0);
    }
    let (generation, read_len, mut copied) = {
        let cache = file_page_cache(key).read();
        let generation = cache.generations.get(&key).copied().unwrap_or(0);
        let Some(file_size) = cache.file_sizes.get(&key).copied() else {
            return FilePageCacheRead::Miss(generation);
        };
        if offset >= file_size {
            return FilePageCacheRead::Hit(0);
        }
        let read_len = buf.len().min(file_size - offset);
        let Some(pages) = cache.pages.get(&key) else {
            return FilePageCacheRead::Miss(generation);
        };

        let mut copied = 0usize;
        let first_chunk_len = read_len.min(FILE_PAGE_READAHEAD_SIZE);
        while copied < first_chunk_len {
            let absolute = offset + copied;
            let page_offset = absolute & !(PAGE_SIZE - 1);
            let within_page = absolute - page_offset;
            let chunk = (first_chunk_len - copied).min(PAGE_SIZE - within_page);
            let Some(frame) = pages.get(&page_offset) else {
                return FilePageCacheRead::Miss(generation);
            };
            let source = (frame.paddr + within_page).slice_with_len::<u8>(chunk);
            buf[copied..copied + chunk].copy_from_slice(source);
            copied += chunk;
        }
        (generation, read_len, copied)
    };

    while copied < read_len {
        let chunk_end = copied
            .saturating_add(FILE_PAGE_READAHEAD_SIZE)
            .min(read_len);
        let cache = file_page_cache(key).read();
        let current_generation = cache.generations.get(&key).copied().unwrap_or(0);
        if current_generation != generation {
            return FilePageCacheRead::Miss(current_generation);
        }
        let Some(pages) = cache.pages.get(&key) else {
            return FilePageCacheRead::Miss(generation);
        };
        while copied < chunk_end {
            let absolute = offset + copied;
            let page_offset = absolute & !(PAGE_SIZE - 1);
            let within_page = absolute - page_offset;
            let chunk = (chunk_end - copied).min(PAGE_SIZE - within_page);
            let Some(frame) = pages.get(&page_offset) else {
                return FilePageCacheRead::Miss(generation);
            };
            let source = (frame.paddr + within_page).slice_with_len::<u8>(chunk);
            buf[copied..copied + chunk].copy_from_slice(source);
            copied += chunk;
        }
    }
    debug_assert_eq!(copied, read_len);
    FilePageCacheRead::Hit(read_len)
}

pub(crate) fn cache_file_read(
    key: FilePageCacheKey,
    generation: usize,
    offset: usize,
    data: &[u8],
    file_size: usize,
) {
    #[cfg(feature = "lock-stats")]
    FILE_PAGE_CACHE_FILL_BYTES.fetch_add(data.len(), Ordering::Relaxed);
    let Some(data_end) = offset.checked_add(data.len()) else {
        return;
    };
    let Some(aligned_offset) = offset.checked_add(PAGE_SIZE - 1) else {
        return;
    };
    let mut page_offset = aligned_offset & !(PAGE_SIZE - 1);
    let page_count = data_end
        .min(file_size)
        .saturating_sub(page_offset)
        .saturating_add(PAGE_SIZE - 1)
        / PAGE_SIZE;
    let mut loaded = Vec::new();
    if loaded.try_reserve_exact(page_count).is_err() {
        publish_file_read_pages(key, generation, file_size, loaded);
        return;
    }
    let frames = frames_alloc_batch(page_count);
    for frame in frames {
        if page_offset >= data_end || page_offset >= file_size {
            break;
        }
        let valid_len = PAGE_SIZE.min(file_size - page_offset);
        if page_offset + valid_len > data_end {
            break;
        }
        let page = frame.paddr.slice_mut_with_len(PAGE_SIZE);
        let source_offset = page_offset - offset;
        page[..valid_len].copy_from_slice(&data[source_offset..source_offset + valid_len]);
        loaded.push((page_offset, frame));
        page_offset += PAGE_SIZE;
    }
    publish_file_read_pages(key, generation, file_size, loaded);
}

pub(crate) fn invalidate_file_page_cache_after_write(
    key: FilePageCacheKey,
    offset: usize,
    len: usize,
    file_size: usize,
) {
    if len == 0 {
        return;
    }
    let Some(end) = offset.checked_add(len) else {
        return;
    };
    let mut cache = file_page_cache(key).write();
    let has_state = cache.generations.contains_key(&key)
        || cache.file_sizes.contains_key(&key)
        || cache.pages.contains_key(&key)
        || cache.readahead.contains_key(&key)
        || cache.in_flight.keys().any(|(load_key, _)| *load_key == key);
    if !has_state {
        return;
    }

    let generation = cache.generations.entry(key).or_insert(0);
    *generation = generation.wrapping_add(1);
    cache.file_sizes.insert(key, file_size);
    cache.readahead.remove(&key);

    let load_keys: Vec<_> = cache
        .in_flight
        .keys()
        .filter(|(load_key, _)| *load_key == key)
        .copied()
        .collect();
    let mut cancelled = Vec::with_capacity(load_keys.len());
    for load_key in load_keys {
        if let Some(load) = cache.in_flight.remove(&load_key) {
            cancelled.push(load.state);
        }
    }

    let first_page = offset & !(PAGE_SIZE - 1);
    let last_page = end.saturating_add(PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
    let page_offsets: Vec<_> = cache
        .pages
        .get(&key)
        .map(|pages| {
            pages
                .range(first_page..last_page)
                .map(|(&page, _)| page)
                .collect()
        })
        .unwrap_or_default();
    let mut retired = Vec::with_capacity(page_offsets.len());
    let mut remove_page_map = false;
    if let Some(pages) = cache.pages.get_mut(&key) {
        for page_offset in page_offsets {
            if let Some(frame) = pages.remove(&page_offset) {
                retired.push(frame);
            }
        }
        remove_page_map = pages.is_empty();
    }
    if remove_page_map {
        cache.pages.remove(&key);
    }
    cache.page_count = cache.page_count.saturating_sub(retired.len());
    drop(cache);
    release_file_page_cache_slots(retired.len());

    #[cfg(feature = "lock-stats")]
    {
        FILE_PAGE_CACHE_RANGE_INVALIDATIONS.fetch_add(1, Ordering::Relaxed);
        FILE_PAGE_CACHE_RANGE_INVALIDATED_PAGES.fetch_add(retired.len(), Ordering::Relaxed);
    }
    for state in cancelled {
        state.store(FILE_PAGE_LOAD_INVALIDATED, Ordering::Release);
        crate::task::wake_file_page_load_waiters(Arc::as_ptr(&state) as usize);
    }
    drop(retired);
}

pub fn invalidate_file_page_cache(key: FilePageCacheKey) {
    let mut cache = file_page_cache(key).write();
    if !cache.generations.contains_key(&key)
        && !cache.file_sizes.contains_key(&key)
        && !cache.pages.contains_key(&key)
        && !cache.readahead.contains_key(&key)
        && !cache.in_flight.keys().any(|(load_key, _)| *load_key == key)
    {
        return;
    }
    let generation = cache.generations.entry(key).or_insert(0);
    *generation = generation.wrapping_add(1);
    cache.file_sizes.remove(&key);
    cache.readahead.remove(&key);
    let retired = cache.pages.remove(&key);
    let retired_count = retired.as_ref().map_or(0, BTreeMap::len);
    #[cfg(feature = "lock-stats")]
    {
        FILE_PAGE_CACHE_INVALIDATIONS.fetch_add(1, Ordering::Relaxed);
        FILE_PAGE_CACHE_INVALIDATED_PAGES.fetch_add(retired_count, Ordering::Relaxed);
    }
    if let Some(ref pages) = retired {
        cache.page_count = cache.page_count.saturating_sub(pages.len());
    }
    let load_keys: Vec<_> = cache
        .in_flight
        .keys()
        .filter(|(load_key, _)| *load_key == key)
        .copied()
        .collect();
    let mut cancelled = Vec::with_capacity(load_keys.len());
    for load_key in load_keys {
        if let Some(load) = cache.in_flight.remove(&load_key) {
            cancelled.push(load.state);
        }
    }
    drop(cache);
    release_file_page_cache_slots(retired_count);
    for state in cancelled {
        state.store(FILE_PAGE_LOAD_INVALIDATED, Ordering::Release);
        crate::task::wake_file_page_load_waiters(Arc::as_ptr(&state) as usize);
    }
    drop(retired);
}
