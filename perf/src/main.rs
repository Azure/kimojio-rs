use kimojio::{
    configuration::{BusyPoll, Configuration},
    operations,
};
use std::{
    alloc::GlobalAlloc,
    time::{Duration, Instant},
};

fn main() {
    pin_self_to_core(2);

    let configuration = Configuration::new().set_busy_poll(BusyPoll::Always);
    kimojio::run_with_configuration(
        0,
        async move {
            let iters = 10000000;
            let mut sum = Duration::from_millis(0);
            let mut min = Duration::MAX;
            let mut max = Duration::from_millis(0);

            // warmup
            for _ in 0..2 {
                operations::nop().await.unwrap();
            }

            A.noalloc(true);

            for _ in 0..iters {
                let start = Instant::now();
                operations::nop().await.unwrap();
                let elapsed = start.elapsed();
                sum += elapsed;
                min = std::cmp::min(min, elapsed);
                max = std::cmp::max(max, elapsed);
            }

            A.noalloc(false);

            println!(
                "Elapsed {sum:?}, per iteration: {:?}, min: {min:?}, max: {max:?}",
                sum / iters
            );
        },
        configuration,
    );
}

/// Affinitize the currently running thread to the given CPU core according to
/// the sched_setaffinity system call
fn pin_self_to_core(cpu: u16) {
    // Pin to a single CPU core
    let mut cpuset: rustix::thread::CpuSet = rustix::thread::CpuSet::new();
    cpuset.set(cpu as usize);
    let _ = rustix::thread::sched_setaffinity(rustix::thread::Pid::from_raw(0), &cpuset);

    // Trigger an explicit yield so the kernel re-schedules onto the target CPU
    rustix::thread::sched_yield();
}

struct MyAllocator {
    system: System,
    noalloc: std::sync::Mutex<bool>,
}

impl MyAllocator {
    pub fn noalloc(&self, noalloc: bool) {
        *self.noalloc.lock().unwrap() = noalloc;
    }
}

unsafe impl GlobalAlloc for MyAllocator {
    unsafe fn alloc(&self, layout: std::alloc::Layout) -> *mut u8 {
        if *self.noalloc.lock().unwrap() {
            panic!("Not supposed to alloc");
        }
        unsafe { self.system.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: std::alloc::Layout) {
        if *self.noalloc.lock().unwrap() {
            panic!("Not supposed to alloc");
        }
        unsafe { self.system.dealloc(ptr, layout) }
    }
}

use std::alloc::System;

#[global_allocator]
static A: MyAllocator = MyAllocator {
    system: System,
    noalloc: std::sync::Mutex::new(false),
};
