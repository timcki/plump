use core::{alloc::Layout, ptr::NonNull};

use rlsf::Tlsf;

// plump patch: FLLEN 16 instead of usize::BITS. the largest block is
// GRANULARITY (8) << FLLEN = 512 KB, more than all of the C3's DRAM, and
// binning below that is unchanged; FLLEN 32 spent 2 KB of free-list
// heads on sizes up to 32 GB
const FLLEN: usize = 16;
type Heap = Tlsf<'static, usize, usize, FLLEN, { usize::BITS as usize }>;

pub(crate) struct TlsfHeap {
    heap: Heap,
    pool_start: usize,
    pool_end: usize,
}

impl TlsfHeap {
    pub unsafe fn new(heap_bottom: *mut u8, size: usize) -> Self {
        let mut heap = Heap::new();

        let block = unsafe { core::slice::from_raw_parts(heap_bottom, size) };
        let actual_size = unsafe { heap.insert_free_block_ptr(block.into()).unwrap() };

        Self {
            heap,
            pool_start: heap_bottom as usize,
            pool_end: heap_bottom as usize + actual_size.get(),
        }
    }

    pub fn size(&self) -> usize {
        self.pool_end - self.pool_start
    }

    pub fn used(&self) -> usize {
        let mut used = 0;
        let pool =
            unsafe { core::slice::from_raw_parts(self.pool_start as *const u8, self.size()) };
        for block in unsafe { self.heap.iter_blocks(NonNull::from(pool)) } {
            if block.is_occupied() {
                used += block.size();
            }
        }
        used
    }

    pub fn free(&self) -> usize {
        let mut free = 0;
        let pool =
            unsafe { core::slice::from_raw_parts(self.pool_start as *const u8, self.size()) };
        for block in unsafe { self.heap.iter_blocks(NonNull::from(pool)) } {
            if !block.is_occupied() {
                free += block.max_payload_size();
            }
        }
        free
    }

    pub fn allocate(&mut self, layout: Layout) -> Option<NonNull<u8>> {
        self.heap.allocate(layout)
    }

    /// `None` when `ptr` is not in this pool; otherwise the result of
    /// the reallocation (`None` inside when out of memory, `ptr` still
    /// valid). plump patch.
    pub(crate) unsafe fn try_reallocate(
        &mut self,
        ptr: NonNull<u8>,
        new_layout: Layout,
    ) -> Option<Option<NonNull<u8>>> {
        let addr = ptr.addr().get();
        if self.pool_start <= addr && self.pool_end > addr {
            Some(unsafe { self.heap.reallocate(ptr, new_layout) })
        } else {
            None
        }
    }

    pub(crate) unsafe fn try_deallocate(&mut self, ptr: NonNull<u8>, layout: Layout) -> bool {
        let addr = ptr.addr().get();
        if self.pool_start <= addr && self.pool_end > addr {
            unsafe { self.heap.deallocate(ptr, layout.align()) };
            true
        } else {
            false
        }
    }
}
