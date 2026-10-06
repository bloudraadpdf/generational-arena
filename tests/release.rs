extern crate generational_arena;
use generational_arena::Arena;
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

struct Counting;

static LIVE: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        LIVE.fetch_add(layout.size(), Ordering::Relaxed);
        System.alloc(layout)
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        System.dealloc(ptr, layout)
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

#[test]
fn a_chunk_of_released_slots_gives_back_its_memory() {
    let mut arena = Arena::new();
    let indices: Vec<_> = (0..100_000u64).map(|i| arena.insert(i)).collect();
    let before = LIVE.load(Ordering::Relaxed);
    for &index in &indices[..90_000] {
        arena.release(index);
    }
    let freed = before - LIVE.load(Ordering::Relaxed);
    // The released slots fill the chunks of 4 KiB before slot 90,000 but at most 1.
    assert!(freed >= 90_000 * 16 - 4_096, "{} bytes freed", freed);
    for &index in &indices[90_000..] {
        assert_eq!(arena[index], index.into_raw_parts().0 as u64);
    }
}
