// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
//! Per-thread statistics for the runtime.
//!
//! TODO: route to Geneva via MDM

use std::cell::Cell;

/// Per-thread statistics for the io_uring runtime.
///
/// Tracks metrics like in-flight I/O operations and task polling counts.
#[derive(Clone, Default)]
pub struct URingStats {
    /// Current number of in-flight polled I/O operations.
    pub in_flight_io_poll: Cell<u64>,
    /// Maximum observed in-flight polled I/O operations.
    pub max_in_flight_io_poll: Cell<u64>,
    /// Current number of in-flight regular I/O operations.
    pub in_flight_io: Cell<u64>,
    /// Maximum observed in-flight regular I/O operations.
    pub max_in_flight_io: Cell<u64>,
    /// Total number of I/O-priority task polls.
    pub tasks_polled_io: Cell<u64>,
    /// Total number of CPU-priority task polls.
    pub tasks_polled_cpu: Cell<u64>,
}

impl URingStats {
    /// Creates a new `URingStats` with all counters at zero.
    pub fn new() -> Self {
        Self::default()
    }

    #[inline(always)]
    pub fn increment_in_flight_io(&self, amount: u64) {
        let value = self.in_flight_io.get() + amount;
        let max_value = self.max_in_flight_io.get();
        self.in_flight_io.set(value);
        self.max_in_flight_io.set(std::cmp::max(value, max_value));
    }

    #[inline(always)]
    pub fn decrement_in_flight_io(&self, amount: u64) {
        self.in_flight_io.set(self.in_flight_io.get() - amount);
    }

    #[inline(always)]
    pub fn increment_in_flight_io_poll(&self, amount: u64) {
        let value = self.in_flight_io_poll.get() + amount;
        let max_value = self.max_in_flight_io_poll.get();
        self.in_flight_io_poll.set(value);
        self.max_in_flight_io_poll
            .set(std::cmp::max(value, max_value));
    }

    #[inline(always)]
    pub fn decrement_in_flight_io_poll(&self, amount: u64) {
        let value = self.in_flight_io_poll.get() - amount;
        self.in_flight_io_poll.set(value);
    }

    #[inline(always)]
    pub fn increment_tasks_polled_io(&self) {
        self.tasks_polled_io.set(self.tasks_polled_io.get() + 1);
    }

    #[inline(always)]
    pub fn increment_tasks_polled_cpu(&self) {
        self.tasks_polled_cpu.set(self.tasks_polled_cpu.get() + 1);
    }
}
