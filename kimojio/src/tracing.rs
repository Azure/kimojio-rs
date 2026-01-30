// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::io_type::IOType;
use uuid::Uuid;

/// A wrapper containing a trace event with context information.
pub struct EventEnvelope {
    /// The index of the thread that generated this event.
    pub thread_index: u8,
    /// The index of the task that generated this event.
    pub task_index: u16,
    /// The trace event.
    pub event: Events,
}

/// Trace events emitted by the runtime for debugging and performance analysis.
pub enum Events {
    TaskScheduled {
        activity_id: Uuid,
        tenant_id: Uuid,
        tag: u32,
        io: bool,
    },
    TaskRunEnd {
        activity_id: Uuid,
        start_time: u64,
        tag: u32,
        cpu: u16,
        action: u8,
        complete: bool,
    },
    RingEnterEnd {
        submissions: u64,
        in_flight_io: u64,
        start_time: u64,
        completions: u64,
        tag: u32,
        task_count: u32,
        want: bool,
        iopoll: bool,
    },
    AsyncWaitStart {
        activity_id: Uuid,
        tag: u32,
    },
    AsyncWaitEnd {
        activity_id: Uuid,
        tag: u32,
    },
    IoStart {
        activity_id: Uuid,
        fd: i32,
        tag: u32,
        io_type: IOType,
    },
    IoEnd {
        activity_id: Uuid,
        result: i32,
        tag: u32,
    },
    IoError {
        activity_id: Uuid,
        tag: u32,
        error: i32,
    },
    #[cfg(feature = "tls")]
    TlsError {
        activity_id: Uuid,
        code: u64,
    },
    FutureCanceled {
        activity_id: Uuid,
    },
}

/// A trait for receiving trace events from the runtime.
///
/// Implement this trait to collect and process runtime trace events.
pub trait TraceConfiguration: Send {
    /// Called when a trace event is emitted.
    fn trace(&self, event: EventEnvelope);
}
