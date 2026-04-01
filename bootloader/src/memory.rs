use alloc::{alloc::dealloc, collections::vec_deque::VecDeque, vec::Vec};
use core::{cmp::Ordering, fmt::Debug, ops::Deref, usize};

use aligned_vec::{AVec, ConstAlign};
use lazy_static::lazy_static;
use log::debug;
use uefi::{
    boot::{AllocateType, MemoryType, PAGE_SIZE, allocate_pages},
    mem::memory_map::{MemoryMap as UefiMemoryMap, MemoryMapMut},
};

use crate::{
    misc::{align_down, align_up},
    protocol::memory::{Arena, ArenaKind},
    resources::{UefiResource, framebuffer::Framebuffer},
};

lazy_static! {
    static ref UEFI_MEMORY_MAP: Vec<OrderedArena> = {
        let mut map = init_memory_map();
        map.sort();
        map
    };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OrderedArena(pub Arena);

impl PartialOrd for OrderedArena {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.0.start.cmp(&other.0.start))
    }
}

impl Ord for OrderedArena {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0.start.cmp(&other.0.start)
    }
}

#[derive(Debug)]
pub struct ArenaMarker(Vec<OrderedArena>);

impl ArenaMarker {
    pub fn new() -> Self {
        Self(Vec::new())
    }

    pub fn push(&mut self, arena: Arena) {
        self.0.push(OrderedArena(arena));
    }

    /// Mark the given memory map with the arenas in this marker,
    /// returning a new memory map with the arenas marked.
    pub fn mark(mut self, mut map: Vec<OrderedArena>) -> Vec<Arena> {
        self.0.sort();
        map.sort();

        unsafe { Self::mark_unchecked(self, map) }
    }

    /// Mark the given memory map with the arenas in this marker, without
    /// sorting.
    ///
    /// This is unsafe because it assumes that the arenas in the marker and the
    /// map are already sorted.
    pub unsafe fn mark_unchecked(self, map: Vec<OrderedArena>) -> Vec<Arena> {
        let mut out: Vec<Arena> = Vec::new();
        let mut marker_cursor = 0;
        let push_and_merge = |map: &mut Vec<Arena>, arena: Arena| {
            if let Some(last) = map.last_mut() {
                if last.end == arena.start && last.kind == arena.kind {
                    last.end = arena.end;
                    return;
                }
            }
            map.push(arena);
        };

        // Arenas in the marker is always a subset of the arenas in the map,
        // so we just iterate over the map and check if the current arena in
        // the marker overlaps with it. If it does, we split the arena in the
        // map into three parts:
        // 1. The part before the marker (if any)
        // 2. The part covered by the marker
        // 3. The part after the marker (if any)
        // We then push these parts into the output vector, and move the cursors
        // accordingly.
        //
        // Also we need to check if the arena in the output vector can be merged with
        // the last one, to avoid fragmentation.
        for OrderedArena(arena) in map.into_iter() {
            let mut current = arena;
            while let Some(OrderedArena(marker)) = self.0.get(marker_cursor) {
                if current.start <= marker.start && current.end >= marker.end {
                    // The marker marked current arena, split it into three parts
                    if current.start < marker.start {
                        push_and_merge(
                            &mut out,
                            Arena {
                                start: current.start,
                                end: marker.start,
                                kind: current.kind,
                            },
                        );
                    }

                    push_and_merge(
                        &mut out,
                        Arena {
                            start: marker.start,
                            end: marker.end,
                            kind: marker.kind,
                        },
                    );

                    if current.end > marker.end {
                        push_and_merge(
                            &mut out,
                            Arena {
                                start: marker.end,
                                end: current.end,
                                kind: current.kind,
                            },
                        );
                    }

                    // Move the cursor forward
                    marker_cursor += 1;

                    // Update current
                    current = out.pop().unwrap();
                } else {
                    // No more markers that overlap with the current arena,
                    // push it to the output vector and break.
                    break;
                }
            }

            push_and_merge(&mut out, current);
        }

        assert!(
            marker_cursor == self.0.len(),
            "Not all marker arenas were used"
        );

        out
    }
}

impl Deref for ArenaMarker {
    type Target = Vec<OrderedArena>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

pub struct MemoryMapBuilder<const PAGE_SIZE: usize = { uefi::boot::PAGE_SIZE }> {
    arena_marker: Option<ArenaMarker>,
}

impl<const PAGE_SIZE: usize> MemoryMapBuilder<PAGE_SIZE> {
    pub fn new() -> Self {
        Self {
            arena_marker: Some(ArenaMarker::new()),
        }
    }

    pub fn add_arena(&mut self, arena: Arena) {
        self.arena_marker.as_mut().unwrap().push(arena);
    }

    pub unsafe fn allocate_and_mark(&mut self, num: usize, kind: ArenaKind) -> usize {
        let addr = allocate_pages(AllocateType::AnyPages, MemoryType::LOADER_DATA, num)
            .expect("Out of system resource.")
            .as_ptr() as usize;

        self.add_arena(Arena {
            start: addr,
            end: addr + num * PAGE_SIZE,
            kind,
        });

        addr
    }

    // TODO: Optimize this function to avoid double iteration, reduce allocations,
    // etc.
    pub fn construct_memory_map(mut self) -> (usize, *mut Arena) {
        let mut uefi_memory_map: Vec<OrderedArena> = UEFI_MEMORY_MAP.clone();
        let mut memmap: AVec<Arena, ConstAlign<PAGE_SIZE>> = AVec::new(PAGE_SIZE);

        // Push framebuffer arena into the memory map if it exists,
        // so that it can be marked as reserved and not used for other purposes.
        if let Some(fb) = Framebuffer::resource() {
            let fb_start = fb.buffer().as_ptr() as usize;
            let fb_end = fb.len() as usize + fb_start;
            uefi_memory_map.push(OrderedArena(Arena {
                start: align_down(PAGE_SIZE, fb_start),
                end: align_up(PAGE_SIZE, fb_end),
                kind: ArenaKind::Framebuffer,
            }));
        }

        let marker = self.arena_marker.take().unwrap();
        let out = marker.mark(uefi_memory_map);
        memmap.extend_from_slice(&out);

        // Push a dummy arena to allocate the memory for the memory map
        memmap.push(Arena {
            start: usize::MAX,
            end: usize::MAX,
            kind: ArenaKind::EndOfMemoryMap,
        });
        memmap.push(Arena {
            start: usize::MAX,
            end: usize::MAX,
            kind: ArenaKind::EndOfMemoryMap,
        });

        let memmap_ptr = memmap.as_mut_ptr();
        let memmap_slice = memmap.as_mut_slice();

        assert!(memmap_ptr as usize % core::mem::align_of::<Arena>() == 0);

        let memmap_arena = Arena {
            start: memmap_ptr as usize,
            end: align_up(
                PAGE_SIZE,
                memmap_ptr as usize + memmap_slice.len() * core::mem::size_of::<Arena>(),
            ),
            kind: ArenaKind::BootloaderProvideInfo,
        };

        for i in 0..memmap_slice.len() {
            if memmap_slice[i].start <= memmap_arena.start
                && memmap_slice[i].end >= memmap_arena.end
            {
                let original = memmap_slice[i];

                let mut new_arenas = VecDeque::new();
                if original.start < memmap_arena.start {
                    new_arenas.push_back(Arena {
                        start: original.start,
                        end: memmap_arena.start,
                        kind: original.kind,
                    });
                }

                new_arenas.push_back(memmap_arena);

                if original.end > memmap_arena.end {
                    new_arenas.push_back(Arena {
                        start: memmap_arena.end,
                        end: original.end,
                        kind: original.kind,
                    });
                }

                memmap_slice[i] = new_arenas.pop_front().unwrap();

                memmap_slice[(i + 1)..].rotate_right(new_arenas.len());
                for (j, new_arena) in new_arenas.into_iter().enumerate() {
                    memmap_slice[i + 1 + j] = new_arena;
                }

                break;
            }
        }

        let ret = (memmap_slice.len(), memmap_ptr);
        core::mem::forget(memmap);

        ret
    }
}

impl Debug for MemoryMapBuilder {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("MemoryMapBuilder")
            .field(
                "arena_marker_count",
                &self.arena_marker.as_ref().map(|m| m.len()),
            )
            .finish()
    }
}

impl<const PAGE_SIZE: usize> Drop for MemoryMapBuilder<PAGE_SIZE> {
    fn drop(&mut self) {
        if let Some(marker) = self.arena_marker.take() {
            for arena in marker.iter() {
                if matches!(
                    arena.0.kind,
                    ArenaKind::ExecutableAndModules
                        | ArenaKind::BootloaderProvideInfo
                        | ArenaKind::KernelPageTable
                        | ArenaKind::KernelStack
                ) {
                    let layout = core::alloc::Layout::from_size_align(
                        arena.0.end - arena.0.start,
                        PAGE_SIZE,
                    )
                    .unwrap();
                    unsafe {
                        dealloc(arena.0.start as *mut u8, layout);
                    }
                }
            }
        }
    }
}

fn init_memory_map() -> Vec<OrderedArena> {
    let map = uefi::boot::memory_map(MemoryType::LOADER_DATA).expect("Unable to obtain memory map");

    map.entries()
        .into_iter()
        .map(|desc| {
            let kind = match desc.ty {
                MemoryType::CONVENTIONAL
                | MemoryType::BOOT_SERVICES_CODE
                | MemoryType::BOOT_SERVICES_DATA
                | MemoryType::RUNTIME_SERVICES_CODE
                | MemoryType::RUNTIME_SERVICES_DATA
                | MemoryType::LOADER_CODE
                | MemoryType::LOADER_DATA => ArenaKind::Usable,
                MemoryType::RESERVED => ArenaKind::Reserved,
                MemoryType::ACPI_RECLAIM => ArenaKind::AcpiReclaimable,
                MemoryType::ACPI_NON_VOLATILE => ArenaKind::AcpiNvs,
                MemoryType::UNUSABLE => ArenaKind::BadMemory,
                unknwon => ArenaKind::Unknown(unknwon.0 as usize),
            };

            OrderedArena(Arena {
                start: desc.phys_start as usize,
                end: (desc.phys_start as usize) + (desc.page_count as usize * PAGE_SIZE),
                kind,
            })
        })
        .collect::<Vec<_>>()
}

pub fn memory_map() -> &'static [OrderedArena] {
    &UEFI_MEMORY_MAP
}
