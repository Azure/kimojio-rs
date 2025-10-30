// Copyright (c) Microsoft Corporation. All rights reserved.
#[cfg(target_arch = "x86_64")]
use std::time::Instant;

static CELL: std::sync::OnceLock<u64> = std::sync::OnceLock::new();

pub struct Timer {}

impl Timer {
    /// Determine the # of timer ticks in a microsecond. On modern x86 CPUs the
    /// TSC timer is stable and usable as a local clock source, see:
    /// 1) nonstop_tsc -> Continues ticking during C-states
    /// 2) constant_tsc -> Consistent frequency
    ///
    /// These CPUID flags indicate the TSC is usable as a clock source
    pub fn ticks_per_us() -> u64 {
        *CELL.get_or_init(Self::compute_ticks_per_us)
    }

    #[inline(always)]
    pub fn ticks() -> u64 {
        #[cfg(target_arch = "x86_64")]
        {
            // SAFETY: _rdtsc is safe to call on x86_64
            unsafe { std::arch::x86_64::_rdtsc() }
        }
        #[cfg(target_arch = "aarch64")]
        {
            // SAFETY: Reading CNTVCT_EL0 is safe on aarch64
            unsafe {
                let mut val: u64;
                std::arch::asm!("mrs {}, cntvct_el0", out(reg) val);
                val
            }
        }
        #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
        {
            // For other architectures, use a fallback (e.g., Instant::now)
            // This is not as precise, but prevents compilation errors.
            Instant::now().as_micros() as u64
        }
    }

    fn compute_ticks_per_us() -> u64 {
        let mut result = u64::MAX;

        #[cfg(target_arch = "x86_64")]
        {
            for _ in 0..64 {
                unsafe {
                    let start_tick = Self::ticks();
                    let start = Instant::now();
                    for _ in 0..4 * 1024 {
                        core::arch::x86_64::_mm_pause();
                    }
                    let total = start.elapsed();
                    let end_tick = Self::ticks();

                    let ticks = end_tick - start_tick;
                    let ticks_per_us = ticks as u128 / total.as_micros();
                    result = std::cmp::min(ticks_per_us as u64, result);
                }
            }
        }
        #[cfg(target_arch = "aarch64")]
        {
            for _ in 0..64 {
                unsafe {
                    let start_tick = Self::ticks();
                    let start = std::time::Instant::now();
                    for _ in 0..4 * 1024 {
                        core::arch::asm!("nop");
                    }
                    let total = start.elapsed();
                    let end_tick = Self::ticks();

                    let ticks = end_tick - start_tick;
                    let ticks_per_us = ticks as u128 / total.as_micros();
                    result = std::cmp::min(ticks_per_us as u64, result);
                }
            }
            #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
            {
                // For other architectures, use a fallback
                result = 1; // 1 million ticks per second, or 1 tick per microsecond
            }
        }
        result
    }
}

#[cfg(test)]
mod test {
    use std::time::Duration;

    use crate::timer::Timer;

    #[test]
    fn test_ticks() {
        let start = Timer::ticks();
        std::thread::sleep(Duration::from_millis(1));
        let now = Timer::ticks();
        let elapsed = now - start;
        let ticks_per_us = Timer::ticks_per_us();
        let elapsed = Duration::from_micros(elapsed / ticks_per_us);
        assert!(elapsed >= Duration::from_millis(0));
    }
}
