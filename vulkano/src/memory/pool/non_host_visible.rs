// Copyright (c) 2016 The vulkano developers
// Licensed under the Apache License, Version 2.0
// <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT
// license <LICENSE-MIT or https://opensource.org/licenses/MIT>,
// at your option. All files in the project carrying such
// notice may not be copied, modified, or distributed except
// according to those terms.

use crate::{
    device::Device,
    memory::{device_memory::MemoryAllocateInfo, DeviceMemory, DeviceMemoryError},
    DeviceSize,
};
use parking_lot::Mutex;
use std::{cmp, ops::Range, sync::Arc};

/// Memory pool that operates on a given memory type.
#[derive(Debug)]
pub struct StandardNonHostVisibleMemoryTypePool {
    device: Arc<Device>,
    memory_type_index: u32,
    // TODO: obviously very inefficient
    occupied: Mutex<Vec<(Arc<DeviceMemory>, Vec<Range<DeviceSize>>)>>,
}

impl StandardNonHostVisibleMemoryTypePool {
    /// Creates a new pool that will operate on the given memory type.
    ///
    /// # Panic
    ///
    /// - Panics if `memory_type_index` is out of range.
    #[inline]
    pub fn new(
        device: Arc<Device>,
        memory_type_index: u32,
    ) -> Arc<StandardNonHostVisibleMemoryTypePool> {
        let _ =
            &device.physical_device().memory_properties().memory_types[memory_type_index as usize];

        Arc::new(StandardNonHostVisibleMemoryTypePool {
            device,
            memory_type_index,
            occupied: Mutex::new(Vec::new()),
        })
    }

    /// Allocates memory from the pool.
    ///
    /// # Panic
    ///
    /// - Panics if `size` is 0.
    /// - Panics if `alignment` is 0.
    ///
    pub fn alloc(
        self: &Arc<Self>,
        size: DeviceSize,
        alignment: DeviceSize,
    ) -> Result<StandardNonHostVisibleMemoryTypePoolAlloc, DeviceMemoryError> {
        assert!(size != 0);
        assert!(alignment != 0);

        #[inline]
        fn align(val: DeviceSize, al: DeviceSize) -> DeviceSize {
            al * (1 + (val - 1) / al)
        }

        // Find a location.
        let mut occupied = self.occupied.lock();

        // Try finding an entry in already-allocated chunks.
        for &mut (ref dev_mem, ref mut entries) in occupied.iter_mut() {
            // Try find some free space in-between two entries.
            for i in 0..entries.len().saturating_sub(1) {
                let entry1 = entries[i].clone();
                let entry1_end = align(entry1.end, alignment);
                let entry2 = entries[i + 1].clone();
                if entry1_end + size <= entry2.start {
                    entries.insert(i + 1, entry1_end..entry1_end + size);
                    return Ok(StandardNonHostVisibleMemoryTypePoolAlloc {
                        pool: self.clone(),
                        memory: dev_mem.clone(),
                        offset: entry1_end,
                        size,
                    });
                }
            }

            // Try append at the end.
            let last_end = entries.last().map(|e| align(e.end, alignment)).unwrap_or(0);
            if last_end + size <= dev_mem.allocation_size() {
                entries.push(last_end..last_end + size);
                return Ok(StandardNonHostVisibleMemoryTypePoolAlloc {
                    pool: self.clone(),
                    memory: dev_mem.clone(),
                    offset: last_end,
                    size,
                });
            }
        }

        // We need to allocate a new block.
        let new_block = {
            const MIN_BLOCK_SIZE: DeviceSize = 8 * 1024 * 1024; // 8 MB
            let allocation_size = cmp::max(MIN_BLOCK_SIZE, size.next_power_of_two());
            let new_block = DeviceMemory::allocate(
                self.device.clone(),
                MemoryAllocateInfo {
                    allocation_size,
                    memory_type_index: self.memory_type_index,
                    ..Default::default()
                },
            )?;
            Arc::new(new_block)
        };

        occupied.push((new_block.clone(), vec![0..size]));
        Ok(StandardNonHostVisibleMemoryTypePoolAlloc {
            pool: self.clone(),
            memory: new_block,
            offset: 0,
            size,
        })
    }

    /// Returns the index of the memory type this pool operates on.
    #[inline]
    pub fn memory_type_index(&self) -> u32 {
        self.memory_type_index
    }
}

#[derive(Debug)]
pub struct StandardNonHostVisibleMemoryTypePoolAlloc {
    pool: Arc<StandardNonHostVisibleMemoryTypePool>,
    memory: Arc<DeviceMemory>,
    offset: DeviceSize,
    size: DeviceSize,
}

impl StandardNonHostVisibleMemoryTypePoolAlloc {
    #[inline]
    pub fn memory(&self) -> &DeviceMemory {
        &self.memory
    }

    #[inline]
    pub fn offset(&self) -> DeviceSize {
        self.offset
    }

    #[inline]
    pub fn size(&self) -> DeviceSize {
        self.size
    }
}

impl Drop for StandardNonHostVisibleMemoryTypePoolAlloc {
    fn drop(&mut self) {
        let mut occupied = self.pool.occupied.lock();

        let entries = occupied
            .iter_mut()
            .find(|e| &*e.0 as *const DeviceMemory == &*self.memory)
            .unwrap();

        entries.1.retain(|e| e.start != self.offset);
    }
}
