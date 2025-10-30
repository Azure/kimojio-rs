// Copyright (c) Microsoft Corporation. All rights reserved.
//! Configuration is used to configure the per thread behavior
//! of the uring runtime.
//!

use std::time::Duration;

#[derive(Default)]
pub struct Configuration {
    /// Adapter around a set of trace buffers which the data path threads will
    /// write trace events to at runtime for debugging and analysis purposes
    pub(crate) trace_buffer_manager: Option<Box<dyn crate::TraceConfiguration>>,

    /// Should the event processing loop busy poll and not block in the kernel
    /// if there are pending IOs. Busy polling avoids the cost of a thread
    /// wakeup when an IO completes, but burns a CPU core instead. Offers
    /// a latency vs. CPU utilization tradeoff. If using this feature, keep it
    /// limited to a small handful of threads, which should be pinned to
    /// specific CPU cores. This is a power user feature that should only be
    /// used with careful testing and experimentation.
    pub(crate) busy_poll: BusyPoll,
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum BusyPoll {
    /// never busy poll, always suspend thread until I/O completes if no tasks are ready
    Never,

    /// always busy poll, never suspend the thread
    Always,

    /// busy poll the thread for Duration, then suspend until an I/O completes
    Until(Duration),
}

impl Default for BusyPoll {
    fn default() -> Self {
        Self::Never
    }
}

impl From<Option<Duration>> for BusyPoll {
    fn from(value: Option<Duration>) -> Self {
        match value {
            None => BusyPoll::Never,
            Some(d) => BusyPoll::Until(d),
        }
    }
}

impl Configuration {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_trace_buffer_manager(
        mut self,
        trace_buffer_manager: Box<dyn crate::TraceConfiguration>,
    ) -> Self {
        self.trace_buffer_manager = Some(trace_buffer_manager);
        self
    }

    pub fn set_busy_poll(mut self, busy_poll: BusyPoll) -> Self {
        self.busy_poll = busy_poll;
        self
    }
}

#[cfg(test)]
mod test {
    use crate::configuration::BusyPoll;

    #[test]
    fn configuration_default_test() {
        let config = crate::Configuration::new();
        assert_eq!(config.busy_poll, BusyPoll::Never);
        assert!(config.trace_buffer_manager.is_none());
    }
}
