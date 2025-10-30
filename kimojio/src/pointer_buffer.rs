// Copyright (c) Microsoft Corporation. All rights reserved.

pub const POINTER_SIZE: usize = std::mem::size_of::<*const ()>();
pub const ZERO_ID: u64 = 0;

/// Convert a pointer value (not the contents) to a buffer of 8 bytes.
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
pub fn pointer_from_buffer<T>(buf: [u8; POINTER_SIZE]) -> Box<T> {
    let buf = buf.as_ptr() as *const *mut T;
    // SAFETY: see pointer_to_buffer. This function should be
    // called exactly one time for each call to pointer_to_buffer.
    unsafe {
        let result = std::ptr::read_unaligned(buf);
        Box::from_raw(result)
    }
}

/// Convert a buffer of 8 bytes to a pointer value.
pub fn pointer_from_buffer_ref<T>(buf: &[u8; POINTER_SIZE]) -> Box<T> {
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
pub struct IdPointerMsg(pub [u8; 2 * POINTER_SIZE]);

impl IdPointerMsg {
    // get the id of the message
    pub fn get_id(&self) -> u64 {
        let mut id_buf = [0_u8; POINTER_SIZE];
        let src = &self.0[0..POINTER_SIZE];
        id_buf.copy_from_slice(src);
        id_from_buffer(id_buf)
    }

    pub fn get_ptr(&self) -> [u8; POINTER_SIZE] {
        let mut ptr_buf = [0_u8; POINTER_SIZE];
        ptr_buf.copy_from_slice(&self.0[POINTER_SIZE..]);
        ptr_buf
    }

    pub fn into_id_box<T>(self) -> (u64, Box<T>) {
        (self.get_id(), pointer_from_buffer(self.get_ptr()))
    }

    pub fn from_id_box<T>(id: u64, pointer: Box<T>) -> Self {
        let mut buff = [0_u8; 2 * POINTER_SIZE];
        let id_buf = id_to_buffer(id);
        let ptr_puf = pointer_to_buffer(pointer);
        let (part1, part2) = buff.split_at_mut(id_buf.len());
        part1.copy_from_slice(&id_buf);
        part2.copy_from_slice(&ptr_puf);
        Self(buff)
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
    use crate::IdPointerMsg;

    use super::{id_from_buffer, id_to_buffer};

    #[test]
    fn test_id_converstion() {
        let id = 99;
        let buf = id_to_buffer(id);
        let id2 = id_from_buffer(buf);
        assert_eq!(id, id2);
    }

    #[test]
    fn test_id_pointer_converstion() {
        let id = 99;
        let mystr = String::from("mystr");
        let data = Box::new(mystr.clone());
        let buff = IdPointerMsg::from_id_box(id, data);
        let (id2, data2) = IdPointerMsg(buff.0).into_id_box::<String>();
        assert_eq!(id, id2);
        assert_eq!(*data2, mystr);
    }
}
