// Copyright (c) Microsoft Corporation. All rights reserved.
//! MutInPlaceCell, RcCell, and ArcCell are zero overhead abstractions
//! for interior mutability. They are all replacements for RefCell.
//!
//! MutInPlaceCell lets you get mutable access, but only from a closure.
//!
//! RcCell and ArcCell only let you set or clone the wrapped `Option<Rc<T>>`
//! or `Option<Arc<T>>`.

use std::{fmt::Debug, marker::PhantomData};

/// MutInPlaceCell is a zero overhead version of RefCell that
/// requires you to make modifications from within a closure
/// to avoid leaking references unsafely.
///
/// # Usage
///
/// ```
/// use kimojio::MutInPlaceCell;
/// let x = MutInPlaceCell::new(0i32);
/// x.use_mut(|x| *x = 5);
///
/// let y = x.use_mut(|x| *x + 1);
/// assert_eq!(y, 6);
/// ```
///
/// Note that you can not return a reference from the closure
/// passed to use_mut, it will be a compile error.
///
/// If you try and call use_mut on a MutInPlaceCell from
/// within a use_mut closure it will panic, by design, since
/// this would allow you to alias the mutable reference.
///
/// # Escaping references
///
/// ```compile_fail
/// let x = kimojio::MutInPlaceCell::new(0i32);
/// let _ignored = x.use_mut(|x| x); // fails
/// ```
///
/// ```compile_fail
/// let x = kimojio::MutInPlaceCell::new(0i32);
/// let mut y = 1i32;
/// let mut z = &mut y;
/// let _ignored = x.use_mut(|x| z = x); //fails
/// ```
///
/// # Send and Sync
///
/// These types are not safe to use from multiple threads
///
/// ```compile_fail
/// fn must_be_sync(_x: &dyn Sync) {}
/// must_be_sync(&kimojio::MutInPlaceCell::new(0i32)); // fails
/// ```
pub struct MutInPlaceCell<T> {
    cell: std::cell::UnsafeCell<T>,
    recursion_check: recursion_check::RecursionCheck,

    // to make MutInPlaceCell not Send
    _marker: PhantomData<*const T>,
}

// SAFETY: MutInPlaceCell is not Sync but it is
// safe to move it from a thread other than where
// it originated from as long as its contents are Send.
unsafe impl<T: Send> Send for MutInPlaceCell<T> {}

impl<T: Default> Default for MutInPlaceCell<T> {
    fn default() -> Self {
        Self::new(Default::default())
    }
}

impl<T: Debug> Debug for MutInPlaceCell<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.use_mut(|item| item.fmt(f))
    }
}

impl<T> MutInPlaceCell<T> {
    pub fn new(value: T) -> Self {
        Self {
            cell: std::cell::UnsafeCell::new(value),
            recursion_check: recursion_check::RecursionCheck::new(),
            _marker: PhantomData,
        }
    }

    #[inline(always)]
    pub fn use_mut<U>(&self, f: impl FnOnce(&mut T) -> U) -> U {
        let _guard = self.recursion_check.enter();
        // SAFETY: We are creating a mutable reference from a const
        // reference. Firstly, we are using UnsafeCell which is a
        // requirement to avoid undefiend behavior. Secondly, we
        // enforce that there is in fact only one mutable reference
        // in the following ways:
        // 1. We do not create const references
        // 2. MutInPlaceCell is not Sync
        // 3. We only pass the mutable reference to a FnOnce so it
        // only lives for as long as the function is called.
        // 4. We panic in debug if use_mut is recursively called from use_mut
        // 5. It is a compile error to return a reference from the FnOnce
        // so the reference can not escape.
        let value = unsafe { &mut *self.cell.get() };
        f(value)
    }
}

#[cfg(debug_assertions)]
mod recursion_check {
    pub struct RecursionCheck(std::cell::Cell<bool>);

    impl RecursionCheck {
        pub fn new() -> Self {
            Self(std::cell::Cell::new(false))
        }
        pub fn enter(&self) -> RecursionCheckGuard<'_> {
            assert!(!self.0.get(), "failed recursion check");
            self.0.set(true);
            RecursionCheckGuard {
                recursion_check: self,
            }
        }
        fn leave(&self) {
            self.0.set(false);
        }
    }

    pub struct RecursionCheckGuard<'a> {
        recursion_check: &'a RecursionCheck,
    }

    impl Drop for RecursionCheckGuard<'_> {
        fn drop(&mut self) {
            self.recursion_check.leave();
        }
    }
}

#[cfg(not(debug_assertions))]
mod recursion_check {
    pub struct RecursionCheck;

    impl RecursionCheck {
        pub fn new() -> Self {
            Self
        }
        pub fn enter(&self) {}
    }
}

#[cfg(test)]
mod test {
    use std::panic::AssertUnwindSafe;

    #[test]
    #[should_panic]
    // only run test in debug builds
    #[cfg(debug_assertions)]
    fn panic_on_recurse_test() {
        let x = super::MutInPlaceCell::new(0i32);
        x.use_mut(|_| x.use_mut(|_| ()));
    }

    #[test]
    fn use_after_panic_test() {
        let x = super::MutInPlaceCell::new(0i32);
        let result = std::panic::catch_unwind(AssertUnwindSafe(|| x.use_mut(|_| panic!("test"))));
        assert!(result.is_err());
        x.use_mut(|_| ());
    }
}
