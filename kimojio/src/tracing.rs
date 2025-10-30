// Copyright (c) Microsoft Corporation. All rights reserved.

use crate::io_type::IOType;
use uuid::Uuid;

pub struct EventEnvelope {
    pub thread_index: u8,
    pub task_index: u16,
    pub event: Events,
}

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
    TlsError {
        activity_id: Uuid,
        code: u64,
    },
    FutureCanceled {
        activity_id: Uuid,
    },
}

pub trait TraceConfiguration: Send {
    fn trace(&self, event: EventEnvelope);
}
