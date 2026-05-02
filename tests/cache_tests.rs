mod test_helpers;

mod cache;

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

struct CountingAllocator;

thread_local! {
    static COUNT_ALLOCATIONS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static THREAD_ALLOCATIONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

unsafe impl std::alloc::GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: std::alloc::Layout) -> *mut u8 {
        COUNT_ALLOCATIONS.with(|enabled| {
            if enabled.get() {
                THREAD_ALLOCATIONS.with(|count| count.set(count.get() + 1));
            }
        });
        unsafe { std::alloc::System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: std::alloc::Layout) {
        unsafe { std::alloc::System.dealloc(ptr, layout) }
    }
}

pub(crate) fn allocation_count_delta<T>(f: impl FnOnce() -> T) -> (T, usize) {
    THREAD_ALLOCATIONS.with(|count| count.set(0));
    COUNT_ALLOCATIONS.with(|enabled| enabled.set(true));
    let result = f();
    COUNT_ALLOCATIONS.with(|enabled| enabled.set(false));
    let allocations = THREAD_ALLOCATIONS.with(std::cell::Cell::get);
    (result, allocations)
}
