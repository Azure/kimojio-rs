// Copyright (c) Microsoft Corporation. All rights reserved.

/// A buffer that can be prefixed with data. The primary use
/// case is when you want to create a protocol stack where
/// each layer can add additional data to the front of the
/// buffer without having to allocate or copy memory.
///
/// There is a similar crate called [buffer](https://docs.rs/buffer/latest/buffer/index.html).
/// However, it only lets you add new data to the end of the buffer.
///
/// Normal usage would be for protocol layers to take an owned
/// reference to a PrefixBuffer which will be passed down to
/// each subsequent layer. The bottom most layer will be ablet
/// to send the accumulated contents in one write. The key
/// is that the initial caller that allocates the buffer must
/// place the initial data, which is typically the application
/// data, in the middle or end of the buffer, such that there is
/// sufficient room left over for the headers to go in prefix
/// position.
///
/// If the initial caller can not provide an owned PrefixBuffer,
/// they can call the `view` method in order to create a reference
/// based PrefixBuffer that the layers can modify without affecting
/// the initial PrefixBuffer.
///
/// # Usage
///
/// ```rust
/// use kimojio::{PrefixBuffer, StaticBuffer};
/// use std::io::{Read, Write};
///
/// // StaticBuffer is implemented with a fixed size array internally.
/// // Use OwnedBuffer for heap allocated buffers, or BufferView to
/// // wrap an existing slice.
/// let mut buffer = StaticBuffer::<1024>::new();
///
/// // read some bytes, but make sure to pass less than 1024 to
/// // reserve so that there is still room in prefix position.
/// let mut file = std::fs::File::open("/dev/zero").unwrap();
/// let actual = file.read(buffer.reserve(800)).unwrap();
///
/// // we reserved 800, but need to trim that to the actual amount we read
/// buffer.keep(actual);
///
/// next_layer(buffer.view());
///
/// fn next_layer(mut buffer: impl PrefixBuffer) {
///     // add some data to the front of the buffer
///     buffer.prefix(b"HEADER");
///
///     // now we can write it to the actual I/O device
///     let mut file = std::fs::File::create("/dev/null").unwrap();
///     file.write_all(buffer.as_slice()).unwrap();
/// }
/// ```
pub trait PrefixBuffer {
    /// Returns the initialized portion of the buffer as a slice.
    fn as_slice(&self) -> &[u8];

    /// Returns the initialized portion of the buffer as a mutable slice.
    fn as_slice_mut(&mut self) -> &mut [u8];

    /// Returns the number of prefix bytes available. This is the largest
    /// amount that can be passed to `reserve` without panicing.
    fn available(&self) -> usize;

    /// Returns the length of the initialized portion of the buffer.
    fn len(&self) -> usize;

    /// Reserves space in the prefix position of the buffer. The caller
    /// must initialize all the bytes in the resulting mutable slice.
    /// If the caller does not initialize all the bytes, then they must
    /// call `take` passing the number of bytes that were initialized.
    /// TODO: enforce with asserts
    fn reserve(&mut self, size: usize) -> &mut [u8];

    /// Returns the specified number of bytes to prefix position. `amount`
    /// must be less than what is returned by `available`.
    fn take(&mut self, amount: usize);

    /// `view` allows you to create a PrefixBuffer to pass to a function
    /// without consuming your owned buffer.
    fn view(&mut self) -> BufferView<'_>;

    /// A convenience method for remove the first four bytes from the
    /// initialized portion of the buffer and interpreting them as a
    /// little endian u32.
    fn take_u32(&mut self) -> u32 {
        let size = std::mem::size_of::<u32>();
        assert!(size <= self.len(), "You can only take as much as you have");
        let result = u32::from_le_bytes(self.as_slice()[0..size].try_into().unwrap());
        self.take(size);
        result
    }

    /// Set the buffer to a state where all bytes are uninitialized.
    fn reset(&mut self) {
        self.take(self.len());
    }

    /// Set the last N bytes to initialized with the value `value`, maximizing
    /// the prefix size. This function will replace all data in the PrefixBuffer.
    fn set(&mut self, value: &[u8]) {
        self.reset();
        self.prefix(value);
    }

    /// Allocate room from the uninitialized bytes just prior to the initialized
    /// bytes with the contents of `value`.
    fn prefix(&mut self, value: &[u8]) -> &mut Self {
        assert!(
            value.len() <= self.available(),
            "You can only prefix as much as you have available"
        );
        self.reserve(value.len()).copy_from_slice(value);
        self
    }

    /// True when there are no initialized bytes in the PrefixBuffer.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// A PrefixBuffer implementation that wraps an existing slice without taking ownership of the underlying buffer
pub struct BufferView<'a> {
    head: usize,
    data: &'a mut [u8],
}

impl<'a> BufferView<'a> {
    /// Create a BufferView from a mutable slice where
    /// no bytes are considered initialized initially.
    pub fn new(data: &'a mut [u8]) -> Self {
        Self {
            head: data.len(),
            data,
        }
    }

    /// Create a BufferView from a mutable slice where
    /// 0..available is uninitialized, and available.. is initialized.
    pub fn with_available(available: usize, data: &'a mut [u8]) -> BufferView<'a> {
        assert!(
            available <= data.len(),
            "available must be less than or equal to the length of the slice"
        );
        Self {
            head: available,
            data,
        }
    }

    /// Create a BufferView from a mutable slice where
    /// the last value.len() bytes are initialized with `value`
    /// and the remaining bytes at the beginning are uninitialized.
    pub fn with_prefix(data: &'a mut [u8], value: &[u8]) -> Self {
        assert!(
            value.len() <= data.len(),
            "the prefix must be smaller than the buffer"
        );
        let head = data.len() - value.len();
        data[head..].copy_from_slice(value);
        Self { head, data }
    }
}

impl PrefixBuffer for BufferView<'_> {
    fn available(&self) -> usize {
        debug_assert!(self.head <= self.data.len());
        self.head
    }

    fn as_slice(&self) -> &[u8] {
        debug_assert!(self.head <= self.data.len());
        &self.data[self.head..]
    }

    fn as_slice_mut(&mut self) -> &mut [u8] {
        debug_assert!(self.head <= self.data.len());
        &mut self.data[self.head..]
    }

    fn len(&self) -> usize {
        debug_assert!(self.head <= self.data.len());
        self.data.len() - self.head
    }

    fn reserve(&mut self, size: usize) -> &mut [u8] {
        assert!(
            size <= self.head,
            "You can only reserve as much as you have available"
        );
        let head = self.head;
        let new_head = head - size;
        let head = self.head;
        let result = &mut self.data[new_head..head];
        self.head = new_head;
        result
    }

    fn take(&mut self, amount: usize) {
        assert!(
            self.head + amount <= self.data.len(),
            "You can only take as much as you have"
        );
        self.head += amount;
    }

    fn view(&mut self) -> BufferView<'_> {
        BufferView {
            head: self.head,
            data: self.data,
        }
    }
}

/// A PrefixBuffer that store a fixed size array without allocating on the heap
pub struct StaticBuffer<const SIZE: usize> {
    head: usize,
    tail: usize,
    data: [u8; SIZE],
}

impl<const SIZE: usize> StaticBuffer<SIZE> {
    /// Create a StaticBuffer where all bytes are uninitialized.
    pub fn new() -> Self {
        Self {
            head: SIZE,
            tail: SIZE,
            data: [0; SIZE],
        }
    }

    /// Create a StaticBuffer where the last N bytes are initialized
    /// with `value`.
    pub fn with(value: &[u8]) -> Self {
        let mut this = Self {
            head: SIZE,
            tail: SIZE,
            data: [0; SIZE],
        };
        this.set(value);
        this
    }

    /// Truncate the initialized portion of the buffer to `size`.
    pub fn keep(&mut self, size: usize) {
        let tail = self.head + size;
        assert!(
            tail <= self.tail,
            "You can only keep as much as you have. size is {} but len() is {}",
            size,
            self.len()
        );
        self.tail = tail;
    }
}

impl<const SIZE: usize> PrefixBuffer for StaticBuffer<SIZE> {
    fn available(&self) -> usize {
        debug_assert!(self.head <= self.tail);
        self.head
    }

    fn as_slice(&self) -> &[u8] {
        debug_assert!(self.head <= self.tail);
        &self.data[self.head..self.tail]
    }

    fn as_slice_mut(&mut self) -> &mut [u8] {
        debug_assert!(self.head <= self.tail);
        &mut self.data[self.head..self.tail]
    }

    fn len(&self) -> usize {
        debug_assert!(self.head <= self.tail);
        self.tail - self.head
    }

    fn reserve(&mut self, size: usize) -> &mut [u8] {
        assert!(
            size <= self.head,
            "You can only reserve as much as you have available"
        );
        let head = self.head;
        let new_head = head - size;
        let head = self.head;
        let result = &mut self.data[new_head..head];
        self.head = new_head;
        result
    }

    fn take(&mut self, amount: usize) {
        assert!(
            self.head + amount <= self.tail,
            "You can only take as much as you have"
        );
        self.head += amount;
    }

    fn reset(&mut self) {
        self.head = self.data.len();
        self.tail = self.data.len();
    }

    fn view(&mut self) -> BufferView<'_> {
        BufferView::with_available(self.head, &mut self.data[..self.tail])
    }
}

impl<const SIZE: usize> Default for StaticBuffer<SIZE> {
    fn default() -> Self {
        Self::new()
    }
}

/// A PrefixBuffer that allocates a fixed size array on the heap
pub struct OwnedBuffer {
    head: usize,
    tail: usize,
    data: Box<[u8]>,
}

impl OwnedBuffer {
    /// Create a OwnedBuffer where all bytes are uninitialized.
    pub fn new(size: usize) -> Self {
        Self {
            head: size,
            tail: size,
            data: vec![0; size].into_boxed_slice(),
        }
    }

    /// Create a OwnedBuffer where the last N bytes are initialized
    /// with `value`.
    pub fn with(size: usize, value: &[u8]) -> Self {
        let mut this = Self::new(size);
        this.set(value);
        this
    }

    /// Truncate the initialized portion of the buffer to `size`.
    pub fn keep(&mut self, size: usize) {
        let tail = self.head + size;
        assert!(
            tail <= self.tail,
            "You can only keep as much as you have. size is {} but len() is {}",
            size,
            self.len()
        );
        self.tail = tail;
    }
}

impl PrefixBuffer for OwnedBuffer {
    fn available(&self) -> usize {
        debug_assert!(self.head <= self.tail);
        self.head
    }

    fn as_slice(&self) -> &[u8] {
        debug_assert!(self.head <= self.tail);
        &self.data[self.head..self.tail]
    }

    fn as_slice_mut(&mut self) -> &mut [u8] {
        debug_assert!(self.head <= self.tail);
        &mut self.data[self.head..self.tail]
    }

    fn len(&self) -> usize {
        debug_assert!(self.head <= self.tail);
        self.tail - self.head
    }

    fn reserve(&mut self, size: usize) -> &mut [u8] {
        assert!(
            size <= self.head,
            "You can only reserve as much as you have available"
        );
        let head = self.head;
        let new_head = head - size;
        let head = self.head;
        let result = &mut self.data[new_head..head];
        self.head = new_head;
        result
    }

    fn take(&mut self, amount: usize) {
        assert!(
            self.head + amount <= self.tail,
            "You can only take as much as you have"
        );
        self.head += amount;
    }

    fn reset(&mut self) {
        self.head = self.data.len();
        self.tail = self.data.len();
    }

    fn view(&mut self) -> BufferView<'_> {
        BufferView::with_available(self.head, &mut self.data[..self.tail])
    }
}

#[cfg(test)]
mod test {
    use crate::{BufferView, OwnedBuffer, StaticBuffer, prefix_buffer::PrefixBuffer};

    pub fn buffer_impl_tests<B: PrefixBuffer>(mut buffer: B, size: usize) {
        let assert_buffer = |buffer: &B, expected: &[u8]| {
            assert_eq!(buffer.len(), expected.len());
            assert_eq!(buffer.as_slice(), expected);
            assert_eq!(buffer.available(), size - buffer.len());
            assert_eq!(buffer.is_empty(), buffer.len() == 0);
        };

        // prefix and reset
        buffer.prefix(&[1, 2, 3]);
        assert_buffer(&buffer, &[1, 2, 3]);
        buffer.prefix(&[4, 5, 6]);
        assert_buffer(&buffer, &[4, 5, 6, 1, 2, 3]);
        buffer.reset();
        assert_buffer(&buffer, &[]);
        buffer.prefix(&[4, 5, 6]);
        assert_buffer(&buffer, &[4, 5, 6]);
        buffer.prefix(&[1, 2, 3]);
        assert_buffer(&buffer, &[1, 2, 3, 4, 5, 6]);

        // take
        buffer.take(2);
        assert_buffer(&buffer, &[3, 4, 5, 6]);

        // reserve
        buffer.reset();
        assert_buffer(&buffer, &[]);
        let reserved = buffer.reserve(3);
        reserved.copy_from_slice(&[1, 2, 3]);
        assert_buffer(&buffer, &[1, 2, 3]);

        // set
        buffer.set(&[1, 0, 0, 0, 5]);
        assert_eq!(buffer.as_slice(), &[1, 0, 0, 0, 5]);

        // take_u32
        let value = buffer.take_u32();
        assert_buffer(&buffer, &[5]);
        assert_eq!(value, 1);
    }

    #[test]
    pub fn buffer_tests() {
        let mut buffer = [0; 768];
        let len = buffer.len();
        let view = BufferView::new(&mut buffer);
        buffer_impl_tests(view, len);
    }

    #[test]
    pub fn owned_buffer_tests() {
        const SIZE: usize = 1024;
        let mut buffer = OwnedBuffer::new(SIZE);

        buffer.set(b"abcd");
        buffer.keep(2);
        assert_eq!(buffer.as_slice(), b"ab");
        buffer.reset();

        buffer_impl_tests(buffer.view(), SIZE);
        buffer_impl_tests(buffer, SIZE);

        let mut buffer = OwnedBuffer::with(SIZE, b"abcd");
        assert_eq!(buffer.as_slice(), b"abcd");
        buffer.as_slice_mut().copy_from_slice(b"1234");
        assert_eq!(buffer.as_slice(), b"1234");
    }

    #[test]
    pub fn static_buffer_tests() {
        const SIZE: usize = 1023;
        let mut buffer = StaticBuffer::<1023>::default();

        buffer.set(b"abcd");
        buffer.keep(2);
        assert_eq!(buffer.as_slice(), b"ab");
        buffer.reset();

        buffer_impl_tests(buffer.view(), SIZE);
        buffer_impl_tests(buffer, SIZE);

        let mut buffer = StaticBuffer::<1023>::with(b"abcd");
        assert_eq!(buffer.as_slice(), b"abcd");
        buffer.as_slice_mut().copy_from_slice(b"1234");
        assert_eq!(buffer.as_slice(), b"1234");
    }

    #[test]
    #[should_panic(expected = "You can only keep as much as you have. size is 21 but len() is 20")]
    pub fn owned_buffer_keep_too_much_test() {
        let mut buffer = OwnedBuffer::new(1024);
        buffer.reserve(20);
        buffer.keep(21);
    }

    #[test]
    pub fn buffer_view_tests() {
        let mut buffer = [0; 256];
        let mut view = BufferView::with_prefix(&mut buffer, b"abcd");
        assert_eq!(view.as_slice(), b"abcd");
        view.as_slice_mut().copy_from_slice(b"1234");
        assert_eq!(view.as_slice(), b"1234");
        let mut view2 = view.view();
        view2.prefix(b"5678");
        assert_eq!(view.as_slice(), b"1234");
    }
}
