// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use rustix_uring::Errno;

/// An error that is returned when attempting to receive a message from a
/// channel that has been closed.
#[derive(PartialEq, Eq, Clone, Copy, Debug)]
pub enum ChannelError {
    Closed,
    Canceled,
    Timeout,
}

impl std::fmt::Display for ChannelError {
    fn fmt(&self, fmt: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        "receiving on a closed channel".fmt(fmt)
    }
}

impl std::error::Error for ChannelError {}

impl From<ChannelError> for Errno {
    fn from(value: ChannelError) -> Self {
        match value {
            ChannelError::Closed => Errno::PIPE,
            ChannelError::Canceled => Errno::CANCELED,
            ChannelError::Timeout => Errno::TIME,
        }
    }
}

impl From<CanceledError> for ChannelError {
    fn from(_: CanceledError) -> Self {
        ChannelError::Canceled
    }
}

impl From<TimeoutError> for ChannelError {
    fn from(e: TimeoutError) -> Self {
        match e {
            TimeoutError::Timeout => ChannelError::Timeout,
            TimeoutError::Canceled => ChannelError::Canceled,
        }
    }
}

/// An error indicating an operation timed out or was canceled.
#[derive(Debug, Copy, Clone, PartialEq)]
pub enum TimeoutError {
    /// The operation exceeded its deadline.
    Timeout,
    /// The operation was canceled.
    Canceled,
}

impl From<CanceledError> for TimeoutError {
    fn from(_: CanceledError) -> Self {
        TimeoutError::Canceled
    }
}

impl From<TimeoutError> for Errno {
    fn from(err: TimeoutError) -> Self {
        match err {
            TimeoutError::Timeout => Errno::TIMEDOUT,
            TimeoutError::Canceled => Errno::CANCELED,
        }
    }
}

/// An error indicating an operation was canceled.
#[derive(Debug, Copy, Clone, PartialEq)]
pub struct CanceledError {}

impl From<CanceledError> for Errno {
    fn from(_: CanceledError) -> Self {
        Errno::CANCELED
    }
}

/// An error from awaiting a task handle.
pub enum TaskHandleError {
    /// The task was canceled before completion.
    Canceled,
    /// The task panicked during execution.
    Panic(Box<dyn std::any::Any + Send + 'static>),
}

impl std::fmt::Debug for TaskHandleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TaskHandleError::Canceled => write!(f, "TaskHandleError::Canceled"),
            TaskHandleError::Panic(_) => write!(f, "TaskHandleError::Panic"),
        }
    }
}

/// This will propagate panics if the task panicked, and otherwise return
/// Result<T, Errno>.
pub fn errno_from_task_handle_result<R>(
    result: Result<Result<R, Errno>, TaskHandleError>,
) -> Result<R, Errno> {
    match result {
        Ok(result) => result,
        Err(TaskHandleError::Canceled) => Err(Errno::CANCELED),
        Err(TaskHandleError::Panic(payload)) => std::panic::resume_unwind(payload),
    }
}
