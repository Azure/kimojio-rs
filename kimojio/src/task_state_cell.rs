// Copyright (c) Microsoft Corporation. All rights reserved.

//! TaskStateCell is a special case of RefCell for the very specific case of
//! TaskState.  To get good performance from the innermost loop of the
//! uringruntime we can not afford the runtime refcounting of RefCell. This
//! wrapper relies on the uringruntime code to very carefully make sure that
//! the mutable borrow held by the main loop is suspended using into_inner
//! any time that task code might need to mutably borrow task_state.
//!
//! TaskStateCell only enforces the invariant of a single borrow at a time in debug
//! builds. It is therefore imperative that unit test coverage exists to catch
//! any violations of this invariant. Turning on the enforcement in release
//! builds causes a 7% regression in the 'parallel nop' bench.

use std::ops::{Deref, DerefMut};

use crate::task::TaskState;

pub struct TaskStateCell {
    cell: std::cell::UnsafeCell<TaskState>,
    #[cfg(debug_assertions)]
    borrowed: std::cell::Cell<bool>,
}

impl TaskStateCell {
    pub fn new(value: TaskState) -> Self {
        Self {
            cell: std::cell::UnsafeCell::new(value),
            #[cfg(debug_assertions)]
            borrowed: std::cell::Cell::new(false),
        }
    }

    pub fn borrow_mut(&self) -> TaskStateCellRef<'_> {
        #[cfg(debug_assertions)]
        {
            if self.borrowed.get() {
                panic!("TaskStateCell borrowed recursively");
            }
            self.borrowed.set(true);
        }

        let value_ref = unsafe { &mut *self.cell.get() };
        TaskStateCellRef {
            value: self,
            value_ref,
        }
    }
}

pub struct TaskStateCellRef<'a> {
    value: &'a TaskStateCell,
    value_ref: &'a mut TaskState,
}

impl<'a> TaskStateCellRef<'a> {
    pub fn into_inner(self) -> &'a TaskStateCell {
        self.value
    }
}

impl Deref for TaskStateCellRef<'_> {
    type Target = TaskState;

    fn deref(&self) -> &Self::Target {
        self.value_ref
    }
}

impl DerefMut for TaskStateCellRef<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.value_ref
    }
}

#[cfg(debug_assertions)]
impl Drop for TaskStateCellRef<'_> {
    fn drop(&mut self) {
        self.value.borrowed.set(false);
    }
}
