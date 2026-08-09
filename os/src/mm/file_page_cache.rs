use super::{frame_alloc, FrameTracker};
use crate::config::PAGE_SIZE;
use crate::fs::{File, FilePageCacheKey};
use crate::sync::PageCacheLock;
use alloc::collections::BTreeMap;

const FILE_PAGE_CACHE_MAX_PAGES: usize = 131_072;

struct FilePageCacheState {
    generations: BTreeMap<FilePageCacheKey, usize>,
    pages: BTreeMap<FilePageCacheKey, BTreeMap<usize, FrameTracker>>,
    page_count: usize,
}

impl FilePageCacheState {
    const fn new() -> Self {
        Self {
            generations: BTreeMap::new(),
            pages: BTreeMap::new(),
            page_count: 0,
        }
    }
}

static FILE_PAGE_CACHE: PageCacheLock<FilePageCacheState> =
    PageCacheLock::new_named(FilePageCacheState::new(), "Vm.file_page_cache");

fn lookup_file_page(key: FilePageCacheKey, offset: usize) -> Result<FrameTracker, usize> {
    let cache = FILE_PAGE_CACHE.lock();
    if let Some(frame) = cache.pages.get(&key).and_then(|pages| pages.get(&offset)) {
        return Ok(frame.shared_clone());
    }
    Err(cache.generations.get(&key).copied().unwrap_or(0))
}

fn publish_file_page(
    key: FilePageCacheKey,
    offset: usize,
    generation: usize,
    loaded: FrameTracker,
) -> FrameTracker {
    let mut cache = FILE_PAGE_CACHE.lock();
    if cache.generations.get(&key).copied().unwrap_or(0) != generation {
        return loaded;
    }
    if let Some(frame) = cache.pages.get(&key).and_then(|pages| pages.get(&offset)) {
        let cached = frame.shared_clone();
        drop(cache);
        drop(loaded);
        return cached;
    }

    let oldest = if cache.page_count >= FILE_PAGE_CACHE_MAX_PAGES {
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

pub(crate) fn load_file_page(
    file: &dyn File,
    offset: usize,
    cacheable: bool,
) -> Option<(FrameTracker, usize)> {
    // Writable MAP_PRIVATE segments remain process-private. Sharing their
    // initial frame requires a fully atomic COW transition across fork and
    // CLONE_VM; keeping only immutable file pages in this cache preserves the
    // useful text/rodata sharing without allowing libc data-page aliases.
    let cache_lookup = cacheable
        .then(|| file.page_cache_key())
        .flatten()
        .map(|key| (key, lookup_file_page(key, offset)));
    if let Some((_, Ok(frame))) = cache_lookup {
        return Some((frame, PAGE_SIZE));
    }

    let frame = frame_alloc()?;
    let page = frame.paddr.slice_mut_with_len(PAGE_SIZE);
    let copied = file.read_at(offset, page).max(0) as usize;
    let copied = copied.min(page.len());
    if copied < page.len() {
        page[copied..].fill(0);
    }

    let frame = match cache_lookup {
        Some((key, Err(generation))) => publish_file_page(key, offset, generation, frame),
        _ => frame,
    };
    Some((frame, copied))
}

pub fn invalidate_file_page_cache(key: FilePageCacheKey) {
    let mut cache = FILE_PAGE_CACHE.lock();
    let generation = cache.generations.entry(key).or_insert(0);
    *generation = generation.wrapping_add(1);
    let retired = cache.pages.remove(&key);
    if let Some(ref pages) = retired {
        cache.page_count = cache.page_count.saturating_sub(pages.len());
    }
    drop(cache);
    drop(retired);
}
