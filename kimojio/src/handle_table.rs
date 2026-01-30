// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! # HandleTable Module
//!
//! The `HandleTable` type is a specialized container designed for managing a collection of
//! values by automatically assigning unique, reusable identifiers (indices)
//! to each element inserted. This structure is particularly useful when you
//! need a dynamically sized collection with efficient slot reuse and minimal memory
//! overhead. Inserting a value and allocating an unused index is O(1) and looking
//! up and removing a value is also O(1).
//!
//! # Usage
//!
//! ```rust
//! use kimojio::{HandleTable, Index};
//!
//! let mut handle_table = HandleTable::new();
//! let idx = handle_table.insert("hello");
//! assert_eq!(handle_table.remove(idx), Some("hello"));
//! assert_eq!(handle_table.remove(idx), None); // Already removed
//! ```
//!

use std::num::NonZeroUsize;

/// A slot in the handle table, either free or containing a value.
pub enum Slot<T> {
    /// A free slot with a pointer to the next free slot.
    Free { next_free: Option<Index> },
    /// A slot containing a value.
    Used { value: T },
}

/// A table that assigns unique indices to values for O(1) insertion and lookup.
pub struct HandleTable<T> {
    data: Vec<Slot<T>>,
    free_list: Option<Index>, // Head of the linked list of free slots, stored as index.
    requests: usize,
}

/// An index into a `HandleTable`.
#[derive(Copy, Clone, Debug, Eq, Hash, Ord, PartialOrd, PartialEq)]
pub struct Index(NonZeroUsize);

impl Index {
    /// Returns the underlying index value.
    pub fn get(&self) -> usize {
        self.0.get()
    }
}

impl From<NonZeroUsize> for Index {
    fn from(value: NonZeroUsize) -> Self {
        Index(value)
    }
}

impl<T> HandleTable<T> {
    /// Creates a new empty `HandleTable`.
    pub fn new() -> Self {
        Self {
            data: Vec::new(),
            free_list: None,
            requests: 0,
        }
    }

    /// Removes all entries from the table.
    pub fn clear(&mut self) {
        self.data.clear();
        self.free_list = None;
        self.requests = 0;
    }

    /// Returns the number of values in the table.
    pub fn len(&self) -> usize {
        self.requests
    }

    /// Insert a value into the vector and return its index.
    pub fn insert(&mut self, value: T) -> Index {
        self.insert_fn(|_| value)
    }

    /// Insert a value into the vector using a function to generate the value
    pub fn insert_fn(&mut self, value_fn: impl FnOnce(Index) -> T) -> Index {
        self.requests += 1;
        if let Some(index) = self.free_list {
            // Reuse slot from free list.
            let offset = index.0.get() - 1;
            let free = std::mem::replace(
                &mut self.data[offset],
                Slot::Used {
                    value: value_fn(index),
                },
            );
            match free {
                Slot::Free { next_free } => {
                    self.free_list = next_free;
                }
                _ => unreachable!(),
            }
            index
        } else {
            // Create a new slot
            let offset = self.data.len();
            let index = Index(NonZeroUsize::new(offset + 1).unwrap());
            self.data.push(Slot::Used {
                value: value_fn(index),
            });
            index
        }
    }

    /// Get a reference to a value by its index
    pub fn get(&self, index: Index) -> Option<&T> {
        let offset = index.0.get() - 1;

        match self.data.get(offset) {
            Some(Slot::Used { value }) => Some(value),
            _ => None,
        }
    }

    /// Remove a value from the vector by its index and return it.
    pub fn remove(&mut self, index: Index) -> Option<T> {
        let offset = index.0.get() - 1;
        if offset >= self.data.len() {
            return None;
        }
        let slot = &mut self.data[offset];
        if matches!(slot, Slot::Free { .. }) {
            return None;
        }
        self.requests -= 1;
        let next_free = self.free_list;
        self.free_list = Some(index);
        let value = std::mem::replace(&mut self.data[offset], Slot::Free { next_free });
        match value {
            Slot::Used { value } => Some(value),
            _ => unreachable!(),
        }
    }

    /// Returns true if there are no requests.
    pub fn is_empty(&self) -> bool {
        self.requests == 0
    }

    /// Returns an iterator of valid indexes.
    pub fn indexes(&self) -> impl Iterator<Item = Index> + use<'_, T> {
        self.data
            .iter()
            .enumerate()
            .filter_map(|(offset, slot)| match slot {
                Slot::Used { .. } => Some(Index(NonZeroUsize::new(offset + 1).unwrap())),
                _ => None,
            })
    }

    pub fn iter(&self) -> impl Iterator<Item = (Index, &T)> {
        self.data
            .iter()
            .enumerate()
            .filter_map(|(offset, slot)| match slot {
                Slot::Used { value } => {
                    Some((Index(NonZeroUsize::new(offset + 1).unwrap()), value))
                }
                _ => None,
            })
    }

    pub fn iter_mut(&mut self) -> impl Iterator<Item = (Index, &mut T)> {
        self.data
            .iter_mut()
            .enumerate()
            .filter_map(|(offset, slot)| match slot {
                Slot::Used { value } => {
                    Some((Index(NonZeroUsize::new(offset + 1).unwrap()), value))
                }
                _ => None,
            })
    }
}

impl<T> Default for HandleTable<T> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use std::{cell::Cell, rc::Rc};

    #[test]
    fn test_insert_and_iter() {
        let mut handle_table = HandleTable::new();
        let values = vec![10, 20, 30, 40, 50];
        let indices = values
            .into_iter()
            .map(|v| handle_table.insert(v))
            .collect::<Vec<_>>();
        for (i, index) in indices.into_iter().enumerate() {
            let stored = handle_table.remove(index).unwrap();
            assert_eq!(stored, [10, 20, 30, 40, 50][i]);
        }
    }

    #[test]
    fn test_remove() {
        let mut handle_table = HandleTable::new();
        let idx1 = handle_table.insert(100);
        let idx2 = handle_table.insert(200);
        // Remove first value.
        assert_eq!(handle_table.remove(idx1), Some(100));
        // Remove second value.
        assert_eq!(handle_table.remove(idx2), Some(200));
        // Removing again returns None.
        assert_eq!(handle_table.remove(idx1), None);
    }

    #[test]
    fn test_reuse_slot() {
        let mut handle_table = HandleTable::new();
        let idx1 = handle_table.insert(1);
        let idx2 = handle_table.insert(2);
        let idx3 = handle_table.insert(3);
        // Remove middle element.
        assert_eq!(handle_table.remove(idx2), Some(2));
        // Removing again returns None.
        assert_eq!(handle_table.remove(idx2), None);
        // Insert new value, should reuse free slot.
        let idx_new = handle_table.insert(4);
        assert_eq!(idx_new, idx2);
        // Verify correct values.
        assert_eq!(handle_table.remove(idx_new), Some(4));
        assert_eq!(handle_table.remove(idx3), Some(3));
        assert_eq!(handle_table.remove(idx2), None);
        assert_eq!(handle_table.remove(idx1), Some(1));
    }

    #[test]
    fn test_drop() {
        let drop_count = Rc::new(Cell::new(0usize));

        struct Dropper {
            drop_count: Rc<Cell<usize>>,
        }

        impl Drop for Dropper {
            fn drop(&mut self) {
                self.drop_count.set(self.drop_count.get() + 1);
            }
        }

        {
            let mut handle_table = HandleTable::new();
            // Insert many values.
            for _ in 0..100 {
                let drop_count = drop_count.clone();
                handle_table.insert(Dropper { drop_count });
            }
            // Remove a few values.
            for i in (0..100).step_by(10) {
                handle_table
                    .remove(Index(NonZeroUsize::new(i + 1).unwrap()))
                    .unwrap();
            }
            // Insert one new value.
            handle_table.insert(Dropper {
                drop_count: drop_count.clone(),
            });
        }

        assert_eq!(101, drop_count.get());
    }

    #[test]
    fn test_new_and_default() {
        let handle_table1 = HandleTable::<i32>::new();
        let handle_table2 = HandleTable::<i32>::default();

        assert_eq!(handle_table1.len(), 0);
        assert_eq!(handle_table2.len(), 0);
        assert!(handle_table1.is_empty());
        assert!(handle_table2.is_empty());
    }

    #[test]
    fn test_clear() {
        let mut handle_table = HandleTable::new();

        // Insert some values
        let idx1 = handle_table.insert("value1");
        let idx2 = handle_table.insert("value2");
        let idx3 = handle_table.insert("value3");

        assert_eq!(handle_table.len(), 3);
        assert!(!handle_table.is_empty());

        // Clear the table
        handle_table.clear();

        assert_eq!(handle_table.len(), 0);
        assert!(handle_table.is_empty());

        // Previous indices should not be accessible
        assert_eq!(handle_table.get(idx1), None);
        assert_eq!(handle_table.get(idx2), None);
        assert_eq!(handle_table.get(idx3), None);

        // Should be able to insert new values
        let new_idx = handle_table.insert("new_value");
        assert_eq!(handle_table.len(), 1);
        assert_eq!(handle_table.get(new_idx), Some(&"new_value"));
    }

    #[test]
    fn test_len_and_is_empty() {
        let mut handle_table = HandleTable::new();

        // Initially empty
        assert_eq!(handle_table.len(), 0);
        assert!(handle_table.is_empty());

        // Add some items
        let idx1 = handle_table.insert(10);
        assert_eq!(handle_table.len(), 1);
        assert!(!handle_table.is_empty());

        let idx2 = handle_table.insert(20);
        assert_eq!(handle_table.len(), 2);
        assert!(!handle_table.is_empty());

        let idx3 = handle_table.insert(30);
        assert_eq!(handle_table.len(), 3);
        assert!(!handle_table.is_empty());

        // Remove one item
        handle_table.remove(idx2);
        assert_eq!(handle_table.len(), 2);
        assert!(!handle_table.is_empty());

        // Remove remaining items
        handle_table.remove(idx1);
        assert_eq!(handle_table.len(), 1);
        assert!(!handle_table.is_empty());

        handle_table.remove(idx3);
        assert_eq!(handle_table.len(), 0);
        assert!(handle_table.is_empty());
    }

    #[test]
    fn test_get() {
        let mut handle_table = HandleTable::new();

        // Test get on empty table
        let invalid_idx = Index(NonZeroUsize::new(1).unwrap());
        assert_eq!(handle_table.get(invalid_idx), None);

        // Insert values and test get
        let idx1 = handle_table.insert("hello");
        let idx2 = handle_table.insert("world");

        assert_eq!(handle_table.get(idx1), Some(&"hello"));
        assert_eq!(handle_table.get(idx2), Some(&"world"));

        // Remove one value and test get
        handle_table.remove(idx1);
        assert_eq!(handle_table.get(idx1), None);
        assert_eq!(handle_table.get(idx2), Some(&"world"));

        // Test get with out-of-bounds index
        let out_of_bounds = Index(NonZeroUsize::new(100).unwrap());
        assert_eq!(handle_table.get(out_of_bounds), None);
    }

    #[test]
    fn test_insert_fn() {
        let mut handle_table = HandleTable::new();

        // Test insert_fn with function that uses the index
        let idx1 = handle_table.insert_fn(|index| format!("value_at_{}", index.get()));
        assert_eq!(handle_table.get(idx1), Some(&"value_at_1".to_string()));

        let idx2 = handle_table.insert_fn(|index| format!("value_at_{}", index.get()));
        assert_eq!(handle_table.get(idx2), Some(&"value_at_2".to_string()));

        // Remove first value and insert another using insert_fn
        handle_table.remove(idx1);
        let idx3 = handle_table.insert_fn(|index| format!("reused_slot_{}", index.get()));

        // Should reuse the first slot
        assert_eq!(idx3, idx1);
        assert_eq!(handle_table.get(idx3), Some(&"reused_slot_1".to_string()));
    }

    #[test]
    fn test_indexes() {
        let mut handle_table = HandleTable::new();

        // Empty table should have no indexes
        let indexes: Vec<_> = handle_table.indexes().collect();
        assert!(indexes.is_empty());

        // Insert some values
        let idx1 = handle_table.insert("a");
        let idx2 = handle_table.insert("b");
        let idx3 = handle_table.insert("c");

        let mut indexes: Vec<_> = handle_table.indexes().collect();
        indexes.sort();
        assert_eq!(indexes, vec![idx1, idx2, idx3]);

        // Remove middle value
        handle_table.remove(idx2);
        let mut indexes: Vec<_> = handle_table.indexes().collect();
        indexes.sort();
        assert_eq!(indexes, vec![idx1, idx3]);

        // Add new value (should reuse slot)
        let idx4 = handle_table.insert("d");
        assert_eq!(idx4, idx2); // Should reuse idx2's slot

        let mut indexes: Vec<_> = handle_table.indexes().collect();
        indexes.sort();
        assert_eq!(indexes, vec![idx1, idx2, idx3]); // idx4 == idx2, so we have 1, 2, 3
    }

    #[test]
    fn test_iter() {
        let mut handle_table = HandleTable::new();

        // Empty table
        let items: Vec<_> = handle_table.iter().collect();
        assert!(items.is_empty());

        // Insert values
        let idx1 = handle_table.insert(10);
        let idx2 = handle_table.insert(20);
        let idx3 = handle_table.insert(30);

        let mut items: Vec<_> = handle_table.iter().collect();
        items.sort_by_key(|&(idx, _)| idx);
        assert_eq!(items, vec![(idx1, &10), (idx2, &20), (idx3, &30)]);

        // Remove one value
        handle_table.remove(idx2);
        let mut items: Vec<_> = handle_table.iter().collect();
        items.sort_by_key(|&(idx, _)| idx);
        assert_eq!(items, vec![(idx1, &10), (idx3, &30)]);
    }

    #[test]
    fn test_iter_mut() {
        let mut handle_table = HandleTable::new();

        // Insert values
        let idx1 = handle_table.insert(10);
        let idx2 = handle_table.insert(20);
        let idx3 = handle_table.insert(30);

        // Modify values through iter_mut
        for (_, value) in handle_table.iter_mut() {
            *value += 1;
        }

        // Check modified values
        assert_eq!(handle_table.get(idx1), Some(&11));
        assert_eq!(handle_table.get(idx2), Some(&21));
        assert_eq!(handle_table.get(idx3), Some(&31));

        // Remove one value and modify again
        handle_table.remove(idx2);
        for (_, value) in handle_table.iter_mut() {
            *value *= 2;
        }

        assert_eq!(handle_table.get(idx1), Some(&22));
        assert_eq!(handle_table.get(idx3), Some(&62));
    }

    #[test]
    fn test_index_methods() {
        let idx = Index(NonZeroUsize::new(42).unwrap());
        assert_eq!(idx.get(), 42);

        // Test From trait
        let nz = NonZeroUsize::new(123).unwrap();
        let idx_from: Index = Index::from(nz);
        assert_eq!(idx_from.get(), 123);

        // Test Index traits (Copy, Clone, Debug, Eq, Hash, Ord, PartialOrd, PartialEq)
        let idx1 = Index(NonZeroUsize::new(1).unwrap());
        let idx2 = Index(NonZeroUsize::new(2).unwrap());
        let idx1_copy = idx1;
        let idx1_clone = idx1;

        assert_eq!(idx1, idx1_copy);
        assert_eq!(idx1, idx1_clone);
        assert_ne!(idx1, idx2);
        assert!(idx1 < idx2);
        assert!(idx2 > idx1);

        // Test Debug
        assert_eq!(format!("{idx1:?}"), "Index(1)"); // Test Hash (basic test that it doesn't panic)
        use std::collections::HashMap;
        let mut map = HashMap::new();
        map.insert(idx1, "value1");
        map.insert(idx2, "value2");
        assert_eq!(map.get(&idx1), Some(&"value1"));
    }

    #[test]
    fn test_edge_cases() {
        let mut handle_table = HandleTable::new();

        // Test removing non-existent index
        let fake_idx = Index(NonZeroUsize::new(999).unwrap());
        assert_eq!(handle_table.remove(fake_idx), None);

        // Test multiple removes of same index
        let idx = handle_table.insert("value");
        assert_eq!(handle_table.remove(idx), Some("value"));
        assert_eq!(handle_table.remove(idx), None);
        assert_eq!(handle_table.remove(idx), None);

        // Test get after remove
        assert_eq!(handle_table.get(idx), None);

        // Test complex reuse scenario
        let idx1 = handle_table.insert("1");
        let idx2 = handle_table.insert("2");
        let idx3 = handle_table.insert("3");

        // Remove in specific order to test free list
        handle_table.remove(idx1);
        handle_table.remove(idx3);

        // New inserts should reuse in LIFO order (last removed first)
        let idx_new1 = handle_table.insert("4");
        assert_eq!(idx_new1, idx3); // Most recently freed

        let idx_new2 = handle_table.insert("5");
        assert_eq!(idx_new2, idx1); // Earlier freed

        // Verify values
        assert_eq!(handle_table.get(idx2), Some(&"2"));
        assert_eq!(handle_table.get(idx_new1), Some(&"4"));
        assert_eq!(handle_table.get(idx_new2), Some(&"5"));
    }
}
