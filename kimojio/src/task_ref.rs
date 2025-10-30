// Copyright (c) Microsoft Corporation. All rights reserved.
//! Waker implementation
//!
use std::rc::Rc;

use crate::Task;
use crate::task::TaskState;

/// wake_task can be used when you already have a &mut TaskState reference. This avoids
/// recursive TaskState::get() calls.
pub fn wake_task(task_state: &mut TaskState, waker: &std::task::Waker) {
    if waker.vtable() == &VTABLE {
        let task = clone_waker_task(waker.data());
        task_state.schedule_io(task);
    } else {
        waker.wake_by_ref()
    }
}

static VTABLE: std::task::RawWakerVTable = std::task::RawWakerVTable::new(
    |task| {
        // clone should create a copy of the waker with its own reference
        // count to the underlying task.
        let task = clone_waker_task(task);
        std::task::RawWaker::new(Rc::into_raw(task) as *const (), &VTABLE)
    },
    |task| {
        // wake will only be called at most once. Its job is to put
        // the assocated task into the ready queue. If wake is called
        // then drop will not be called. It is an optimization over
        // wake_by_ref followed by drop.
        let task = consume_waker_task(task);
        let mut task_state = TaskState::get();
        task_state.schedule_io(task);
    },
    |task| {
        // wake_by_ref can potentially be called multiple times.
        // It needs to make the task as ready by moving it to the
        // ready queue. drop or wake could still be called afterwards.
        let task = clone_waker_task(task);
        let mut task_state = TaskState::get();
        task_state.schedule_io(task);
    },
    |task| {
        // drop will only be called if wake isn't
        let _task = consume_waker_task(task);
    },
);

fn clone_waker_task(task: *const ()) -> Rc<Task> {
    let task = task as *const Task;
    // SAFETY: On exit the total reference count
    // will be one greater than it was. This indicates
    // that the waker still owns its reference
    // and the returned Rc<Task> owns the new count.
    unsafe {
        Rc::increment_strong_count(task);
        Rc::from_raw(task)
    }
}

fn consume_waker_task(task: *const ()) -> Rc<Task> {
    let task = task as *const Task;
    // SAFETY: the waker will no longer own its
    // reference so the refcount will not change.
    unsafe { Rc::from_raw(task) }
}

pub fn create_waker(task: Rc<Task>) -> std::task::Waker {
    let raw: std::task::RawWaker =
        std::task::RawWaker::new(Rc::into_raw(task) as *const (), &VTABLE);
    // SAFETY: this unsafe is meant to ensure that you know you have
    // to upload the waker contract, which is that when a waker "wake"
    // is called, poll is guaranteed to eventually be called on the
    // related task.
    unsafe { std::task::Waker::from_raw(raw) }
}
