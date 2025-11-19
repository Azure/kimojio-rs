// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

pub const POINTER_SIZE: usize = std::mem::size_of::<*const ()>();
pub const ZERO_ID: u64 = 0;

/// Convert a pointer value (not the contents) to a buffer of 8 bytes.
///
/// WARNING
///
/// To prevent leaks, the resulting buffer must be used exactly once
/// to recreate the pointer.
pub fn pointer_to_buffer<T>(pointer: Box<T>) -> [u8; POINTER_SIZE] {
    let mut buffer = [0u8; POINTER_SIZE];
    let buffer_ptr = buffer.as_mut_ptr() as *mut *mut T;
    let pointer = Box::into_raw(pointer);
    // SAFETY: We need to enforce that pointer has only one owner and
    // no references. We accomplish that by having pointer parameter
    // be owned. It is the responsibility of the caller to call
    // call pointer_from_buffer with the bytes returned from this
    // function exactly one time (not zero or more than one time).
    unsafe {
        std::ptr::write_unaligned(buffer_ptr, pointer);
    }
    buffer
}

/// Convert a buffer of 8 bytes to a pointer value.
///
/// SAFETY
///
/// The buffer must have been created by a prior call to pointer_to_buffer.
/// A given buffer must never be turned back into a pointer more than once.
/// To avoid leaks it must be turned back into a pointer exactly one time.
pub unsafe fn pointer_from_buffer<T>(buf: [u8; POINTER_SIZE]) -> Box<T> {
    let buf = buf.as_ptr() as *const *mut T;
    // SAFETY: see pointer_to_buffer. This function should be
    // called exactly one time for each call to pointer_to_buffer.
    unsafe {
        let result = std::ptr::read_unaligned(buf);
        Box::from_raw(result)
    }
}

/// Convert a buffer of 8 bytes to a pointer value.
///
/// SAFETY
///
/// The buffer must have been created by a prior call to pointer_to_buffer.
/// A given buffer must never be turned back into a pointer more than once.
/// To avoid leaks it must be turned back into a pointer exactly one time.
pub unsafe fn pointer_from_buffer_ref<T>(buf: &[u8; POINTER_SIZE]) -> Box<T> {
    let buf = buf.as_ptr() as *const *mut T;
    // SAFETY: see pointer_to_buffer. This function should be
    // called exactly one time for each call to pointer_to_buffer.
    unsafe {
        let result = std::ptr::read_unaligned(buf);
        Box::from_raw(result)
    }
}

/// The msg used by IdMessagePipe protocol to send on pipe.
/// First 8 bytes is the request id u64, and second 8 bytes
/// is the raw pointer value.
pub struct IdPointerMsg<T>(Option<[u8; 2 * POINTER_SIZE]>, std::marker::PhantomData<T>);

impl<T> IdPointerMsg<T> {
    pub fn new(data: [u8; 2 * POINTER_SIZE]) -> Self {
        Self(Some(data), std::marker::PhantomData)
    }

    pub fn into_id_box(mut self) -> (u64, Box<T>) {
        let data = self.0.take().unwrap();
        let id = id_from_buffer(data[0..POINTER_SIZE].try_into().unwrap());
        let ptr_src = data[POINTER_SIZE..POINTER_SIZE * 2].try_into().unwrap();
        let ptr = unsafe {
            // SAFETY: take() ensures that this is not called more than once on the buffer
            pointer_from_buffer(ptr_src)
        };
        (id, ptr)
    }

    pub fn from_id_box(id: u64, pointer: Box<T>) -> Self {
        let mut buff = [0_u8; 2 * POINTER_SIZE];
        let id_buf = id_to_buffer(id);
        let ptr_puf = pointer_to_buffer(pointer);
        let (part1, part2) = buff.split_at_mut(id_buf.len());
        part1.copy_from_slice(&id_buf);
        part2.copy_from_slice(&ptr_puf);
        Self::new(buff)
    }
}

impl<T> AsRef<[u8]> for IdPointerMsg<T> {
    fn as_ref(&self) -> &[u8] {
        self.0.as_ref().unwrap()
    }
}

impl<T> Drop for IdPointerMsg<T> {
    fn drop(&mut self) {
        // if dropped without calling from_id_box, prevent leak
        if let Some(data) = self.0.take() {
            let ptr_src = data[POINTER_SIZE..POINTER_SIZE * 2].try_into().unwrap();
            unsafe {
                // SAFETY: take() ensures that this is not called more than once on the buffer
                let _item_to_drop = pointer_from_buffer::<T>(ptr_src);
            }
        }
    }
}

/// converts the id to buffer.
fn id_to_buffer(id: u64) -> [u8; POINTER_SIZE] {
    id.to_be_bytes()
}

/// converts buffer to id
fn id_from_buffer(buf: [u8; POINTER_SIZE]) -> u64 {
    u64::from_be_bytes(buf)
}

#[cfg(test)]
mod tests {
    use super::IdPointerMsg;

    use super::{id_from_buffer, id_to_buffer};

    #[test]
    fn test_id_conversion() {
        let id = 99;
        let buf = id_to_buffer(id);
        let id2 = id_from_buffer(buf);
        assert_eq!(id, id2);
    }

    #[test]
    fn test_id_pointer_conversion() {
        let id = 99;
        let mystr = String::from("mystr");
        let data = Box::new(mystr.clone());
        let buff = IdPointerMsg::from_id_box(id, data);
        let (id2, data2) = buff.into_id_box();
        assert_eq!(id, id2);
        assert_eq!(*data2, mystr);
    }
}
