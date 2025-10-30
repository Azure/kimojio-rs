// Copyright (c) Microsoft Corporation. All rights reserved.
//! Per-thread statistics for the runtime.
//!
//! TODO: route to Geneva via MDM

use std::cell::Cell;

#[derive(Clone, Default)]
pub struct URingStats {
    pub in_flight_io_poll: Cell<u64>,
    pub in_flight_io: Cell<u64>,
    pub max_in_flight_io: Cell<u64>,
    pub tasks_polled_io: Cell<u64>,
    pub tasks_polled_cpu: Cell<u64>,
}

impl URingStats {
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
        self.in_flight_io_poll.set(value);
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
