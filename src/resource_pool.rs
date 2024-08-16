/// This is an attempt at controlling memory usage
/// Please read docs on the allocator and reclaim method. 
/// This is a pretty heavy-weight mechanism of limiting the memory consumption
/// This module is current not being used in main luna, but is kept as a record of what has been worked on. 
/// We still may want a resource pool later. 
use dashmap::DashMap;
use std::alloc::{alloc, Layout};
use std::cmp::Ordering;
use std::collections::BTreeMap;
use thiserror::Error;
use derive_more::Debug;
use std::ptr;
use std::sync::Arc;
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};
use hyper::{Request, Uri};
use log::{trace};
use parking_lot::{Condvar, Mutex};
use serde::{Serialize};
use serde_derive::{Deserialize};
use std::str::FromStr;
use reqwest::Url;
use crate::observable::Observable;
use crate::yaml_serializable::{JsonSerializable, YamlSerializable};

#[derive(Error, derive_more::Debug)]
pub enum ResourcePoolError {
    #[error("Failed to allocate resources")]
    AllocationFailed,
    #[error("Resource pool has no more resources to allocate.")]
    ResourcePoolOutOfMemory(ResourceRequest),
    #[error("Resource pool found a suitable block of memory but it was unexpectedly unavailable")]
    ResourcesFoundButUnexpectedlyUnavailable,
    #[error("Allocated block of memory already in use.")]
    AllocatedBlockAlreadyInUse,
    #[error("Pointer to memory was unexpectedly found in the ordering map.")]
    UnallocatedMemoryUnexpectedlyInOrderingMap,
    #[error("Error occured whilst allocating memory from the resource pool")]
    UnknownError,
}

#[derive(derive_more::Debug, Clone, Eq, PartialEq)]
pub enum ResourceRequestState {
    Filled,
    Waiting,
}

#[derive(Debug, Eq, Clone)]
pub struct Resource {
    ptr: *mut u64,
    size: usize,
}

impl Resource {
    pub fn new(ptr: *mut u64, size: usize) -> Self {
        Self { ptr, size }
    }
}

impl Ord for Resource {
    fn cmp(&self, other: &Self) -> Ordering {
        // First compare by size
        match self.size.cmp(&other.size) {
            Ordering::Equal => {
                // If sizes are equal, compare by pointer value
                self.ptr.cmp(&other.ptr)
            }
            other => other,
        }
    }
}

impl PartialOrd for Resource {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for Resource {
    fn eq(&self, other: &Self) -> bool {
        self.size == other.size && self.ptr == other.ptr
    }
}

unsafe impl Send for Resource {}
unsafe impl Sync for Resource {}


#[derive(derive_more::Debug, Clone, PartialOrd, PartialEq, Eq, Ord)]
struct ResourceRequest {
    size: usize,
    // condvar: Condvar
}

impl ResourceRequest {
    pub fn new(size: usize) -> Self {
        Self {
            size
        }
    }
}

#[derive(derive_more::Debug, Clone)]
pub struct ResourceResponse {
    pub resource: Resource,
    pub state: Arc<Mutex<ResourceRequestState>>,
    pub condvar: Arc<Condvar>,
}

impl ResourceResponse {
    fn new_filled(resource: Resource) -> Self {
        Self {
            resource,
            state: Arc::new(Mutex::new(ResourceRequestState::Filled)),
            condvar: Arc::new(Condvar::new()),
        }
    }

    fn new_waiting(resource: Resource) -> Self {
        Self {
            resource,
            state: Arc::new(Mutex::new(ResourceRequestState::Waiting)),
            condvar: Arc::new(Condvar::new()),
        }
    }

    /// Waits until the resource request state is `Filled`.
    pub fn wait_until_filled(&self) {
        let mut state_guard = self.state.lock();
        if *state_guard != ResourceRequestState::Filled {
            trace!("Waiting on condition variable");
            self.condvar.wait(&mut state_guard);
            trace!("Woke from condition variable");
        }
    }

    /// Notify the waiting thread that the state is now `Filled`.
    pub fn notify_filled(&self) {
        let mut state_guard = self.state.lock();
        *state_guard = ResourceRequestState::Filled;
        self.condvar.notify_one();
    }

    pub fn get_resource_ptr(&self) -> *mut u64 {
        self.resource.ptr
    }

    pub fn get_resource(&self) -> &Resource {
        &self.resource
    }
}

impl Ord for ResourceResponse {
    fn cmp(&self, other: &Self) -> Ordering {
        self.resource.cmp(&other.resource)
    }
}

impl PartialOrd for ResourceResponse {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for ResourceResponse {
    fn eq(&self, other: &Self) -> bool {
        self.resource == other.resource
    }
}

impl Eq for ResourceResponse {}

#[derive(derive_more::Debug, Serialize, Deserialize)]
pub struct ResourcePool {
    /// the maximum number of bytes stored by the resource pool
    max_capacity: usize,
    /// The amount of bytes currently stored by the resource pool.
    /// Equals the sum of current_amount_in_use + current_amount_in_reserve
    // current_size: usize,
    /// The amount of memory in bytes allocated and in use
    amount_in_use: usize,
    /// the amount of memoroy in bytes reclaimed by the resource pool, but not deallocated
    /// and available for reuse
    amount_in_reserve: usize,
    /// The amount of data that callers have requested from the ResourcePool.
    amount_in_waiting: usize,
    /// The percentage full a buffer needs to be in order to be recycled. 
    /// It is inefficient to reallocate a buffer of 1024bytes (for instance) for a 
    /// request for only 64 bytes. Must be a number between 0 and 1.
    recycling_threshold: f32,
    /// ptr to buffer. The buffer allocated is a raw bytes buffer that can be used for anything. 
    /// The Vec manages the lifecycle of the buffer
    #[debug(skip)]
    #[serde(skip)]
    in_use: DashMap<*mut u64, Vec<u8>>,
    /// stores memory in use. 
    #[debug(skip)]
    #[serde(skip)]
    reserve: DashMap<*mut u64, Vec<u8>>,
    /// Stores the ordering
    /// BTreeMap is sorted by size, which has to be a key, not a value. 
    /// So value can be empty
    #[debug(skip)]
    #[serde(skip)]
    order: BTreeMap<Resource, ()>,
    /// Store the requests for resources that cannot be filled due to space limitations.
    #[debug(skip)]
    #[serde(skip)]
    wait_queue: BTreeMap<ResourceRequest, Arc<ResourceResponse>>,
    #[debug(skip)]
    #[serde(skip)]
    shutdown_signal: Arc<(Mutex<bool>, Condvar)>, // Used to signal the background thread to shut down

}

impl ResourcePool {
    pub fn new(max_capacity: usize) -> Self {
        let pool = Self {
            max_capacity,
            // current_size: 0,
            amount_in_use: 0,
            amount_in_reserve: 0,
            amount_in_waiting: 0,
            recycling_threshold: 0.8,
            // efficiency: 0.0,
            in_use: DashMap::new(),
            reserve: DashMap::new(),
            order: BTreeMap::new(),
            wait_queue: BTreeMap::new(),
            shutdown_signal: Arc::new((Mutex::new(false), Condvar::new())),
        };
        // pool.start_background_thread();
        pool
    }

    // fn start_background_thread(&self) {
    //     let shutdown_signal = Arc::clone(&self.shutdown_signal);
    //     let reserve = Arc::clone(&self.reserve);
    //     let wait_queue = Arc::clone(&self.wait_queue);
    //     let recycling_threshold = self.recycling_threshold;
    // 
    //     thread::spawn(move || {
    //         let (lock, cvar) = &*shutdown_signal;
    //         loop {
    //             // Check for the shutdown signal
    //             let shutdown = {
    //                 let lock = lock.lock();
    //                 *lock
    //             };
    //             if shutdown {
    //                 break;
    //             }
    // 
    //             // Attempt to match blocks from the reserve to waiting requests
    //             let mut matched = false;
    //             let mut cursor = wait_queue.lock().unwrap().iter();
    // 
    //             while let Some((request, response)) = cursor.next() {
    //                 let mut reserve_cursor = reserve.lock().unwrap().iter();
    // 
    //                 while let Some((resource_key, _)) = reserve_cursor.next() {
    //                     let proportion_full = request.size as f32 / resource_key.size as f32;
    // 
    //                     if proportion_full >= recycling_threshold {
    //                         // Match found, process allocation (reuse logic)
    //                         // ...
    // 
    //                         matched = true;
    //                         break;
    //                     }
    //                 }
    // 
    //                 if matched {
    //                     break;
    //                 }
    //             }
    // 
    //             // Sleep for a short duration to prevent busy-waiting
    //             thread::sleep(Duration::from_millis(100));
    //         }
    //     });
    // }

    /// Calculate and update the efficiency value
    fn calculate_efficiency(&self) -> f32 {
        if self.current_size() == 0 {
            0.0
        } else {
            self.amount_in_use as f32 / self.current_size() as f32
        }
    }

    pub fn shutdown(&self) {
        let (lock, cvar) = &*self.shutdown_signal;
        let mut shutdown = lock.lock();
        *shutdown = true;
        cvar.notify_one();
    }
    
    pub fn with_threshold(max_capacity: usize, recycling_threshold: f32) -> Self {
        /// todo implement the check that recycling threshold is between 0 and 1.
        let mut self_ = Self::new(max_capacity);
        self_.recycling_threshold = recycling_threshold;
        self_
    }

    pub fn current_size(&self) -> usize {
        self.amount_in_use + self.amount_in_reserve
    }

    pub fn wait_list_empty(&self) -> bool {
        self.wait_queue.is_empty()
    }

    pub fn wait_list_size(&self) -> usize {
        self.wait_queue.len()
    }

    /// # Algorithm: Efficient Resource Reclamation with Threshold-Based Matching
    ///
    /// ## Objective:
    /// To efficiently match a reclaimed memory block with the most suitable request in a sorted waiting queue, while ensuring that the recycling threshold is met.
    ///
    /// ## Inputs:
    /// - `reclaimed_block_size`: The size of the reclaimed memory block (e.g., 1000 bytes).
    /// - `waiting_queue`: A `BTreeMap` containing memory requests, sorted by request size.
    /// - `recycling_threshold`: The minimum proportion of the reclaimed block that must be used by a request for it to be considered a good match (e.g., 0.8 or 80%).
    ///
    /// ## Steps:
    ///
    /// 1. **Locate Insertion Position:**
    ///    - Use the logarithmic search properties of the `BTreeMap` to find the position where a request for the `reclaimed_block_size` would be inserted if it were in the queue.
    ///    - This position will give you the smallest request in the queue that can fit into the reclaimed block, or the position just before where it would be placed.
    ///
    /// 2. **Start Backward Search:**
    ///    - Start from the largest request in the queue that is smaller than or equal to the `reclaimed_block_size`.
    ///    - This is done by iterating backward from the located position.
    ///
    /// 3. **Check Recycling Threshold:**
    ///    - Calculate the proportion full for the request:
    ///      `proportion_full = request_size / reclaimed_block_size`
    ///    - If the `proportion_full` meets or exceeds the `recycling_threshold`, select this request as the match for the reclaimed block and stop further searching.
    ///
    /// 4. **Early Exit:**
    ///    - If the first checked request (the largest possible fit) does not meet the threshold, terminate the search immediately.
    ///    - There is no need to check smaller requests, as they will have a lower proportion full and will also fail to meet the threshold.
    ///
    /// 5. **Allocation:**
    ///    - Allocate the reclaimed block to the request that meets the threshold.
    ///    - If no requests meet the threshold, the reclaimed block remains in reserve.
    ///
    /// ## Concrete Example:
    ///
    /// - **Reclaimed Block:** 1000 bytes
    /// - **Requests in Queue:**
    ///   - Request A: 512 bytes
    ///   - Request B: 800 bytes
    ///   - Request C: 900 bytes
    ///   - Request D: 950 bytes
    ///   - Request E: 1052 bytes
    /// - **Recycling Threshold:** 80% (0.8)
    ///
    /// 1. **Locate Insertion Position:**
    ///    - The algorithm finds that a request for 1000 bytes would be inserted between Request D (950 bytes) and Request E (1052 bytes).
    ///
    /// 2. **Backward Search:**
    ///    - Start with Request D (950 bytes) and check its proportion full:
    ///      proportion_full = 950 / 1000 = 0.95 (95%)
    ///    - Since 95% is greater than the threshold of 80%, Request D is selected as the match.
    ///    - No further checks are needed, and the search terminates.
    ///
    /// 3. **Early Exit:**
    ///    - If the threshold were higher (e.g., 97%), Request D would fail the check, and the algorithm would stop without checking Requests C, B, or A, because they are smaller and would also fail the threshold.
    ///
    /// 4. **Outcome:**
    ///    - The 1000 bytes block is allocated to Request D (950 bytes).
    ///
    /// ## Complexity:
    ///
    /// - **Locating the Insertion Position:**
    ///   - This step leverages the binary search properties of `BTreeMap`, which has a time complexity of `O(log n)`.
    ///
    /// - **Backward Search and Threshold Check:**
    ///   - In the worst case, this step could involve a linear scan of the remaining elements, but since it stops early if the threshold is not met, it typically runs in `O(1)` after finding the largest suitable request.
    ///
    /// - **Overall Complexity:**
    ///   - The overall complexity of this algorithm is dominated by the logarithmic search, making it `O(log n)` in typical cases. The backward search is optimized to minimize unnecessary checks, maintaining efficient performance.
    pub(crate) fn reclaim(&mut self, ptr: *mut u64) {
        if let Some((ptr2, mut block)) = self.in_use.remove(&ptr) {
            self.send_to_server();
            assert_eq!(ptr, ptr2); // sanity check
            let size_of_block_available = block.len();
            trace!("size_of_block_available: {:?}", size_of_block_available);
            trace!("Pool before: {:?}", self);
            self.amount_in_use -= size_of_block_available;
            trace!("Pool after: {:?}", self);
    
            let resource = Resource::new(ptr, size_of_block_available);
            self.order.insert(resource.clone(), ());

            // Create a key with the available size and a null pointer
            let key_to_find = ResourceRequest::new(size_of_block_available);

            // Locate the position in the BTreeMap where a request of size size_of_block_available would be inserted
            let mut cursor = self.wait_queue.range(..=key_to_find).rev();

            let mut selected_request: Option<(ResourceRequest, Arc<ResourceResponse>)> = None;

            // Start the backward search from the closest smaller or equal request
            if let Some((resource_request, resource_response)) = cursor.next() {
                let proportion_full: f32 = resource_request.size as f32 / size_of_block_available as f32;

                if proportion_full >= self.recycling_threshold {
                    selected_request = Some((resource_request.clone(), resource_response.clone()));
                }
            }

            if let Some((request, resp)) = selected_request {
                // Process the selected request
                self.amount_in_waiting -= request.size;
    
                // Shrink the block to fit the request size
                block.shrink_to(request.size);
    
                // Adjust accounting metrics after shrinking
                let shrunk_size = block.len();
                self.amount_in_reserve -= size_of_block_available - shrunk_size;
                self.amount_in_use += shrunk_size;
    
                // Assign the correct ptr value to the Response
                let mut resp_raw: *mut ResourceResponse = Arc::as_ptr(&resp) as *mut ResourceResponse;
                unsafe {
                    (*resp_raw).resource.ptr = ptr;
                }
    
                // Remove from (reserve, order) and insert back into "in-use"
                self.order.remove(&resource);
    
                // Finally, insert the block into the "in-use" map
                self.in_use.insert(ptr, block);
    
                trace!("Inside reclaim fn: locking response with memory addr: {:?}; ptr is: {:?}", &resp, ptr);
                let mut locked_state = resp.state.lock();
    
                *locked_state = ResourceRequestState::Filled;
                resp.condvar.notify_one();
            } else {
                // If no request matches, move the block to the reserve
                self.amount_in_reserve += size_of_block_available;
                self.reserve.insert(ptr, block);
            }
        }
    }


    /// # Algorithm: Memory Allocation in the ResourcePool
    /// 
    /// ## Objective:
    /// The allocation algorithm in the `ResourcePool` is designed to efficiently manage memory 
    /// by either (preferably) reusing existing blocks from the reserve or allocating fresh 
    /// memory when necessary. The algorithm ensures that memory is allocated in a way that 
    /// minimizes waste and avoids unnecessary memory fragmentation.
    ///
    /// ## Summary
    /// This algorithm ensures efficient memory management within the `ResourcePool`, 
    /// balancing the reuse of existing memory blocks with the need to allocate 
    /// fresh memory when necessary.
    /// 
    /// ## Inputs:
    /// - `requested_buffer_size`: The size of the memory block requested by the caller.
    /// - `max_capacity`: The maximum amount of memory that the resource pool can manage.
    /// - `recycling_threshold`: The minimum proportion of an available block that must be used to be considered for recycling.
    /// 
    /// ## Steps:
    /// 
    /// 1. **Check for Existing Resources**:
    ///     - The algorithm first checks if there is a suitable memory block in the reserve that can be reused.
    ///     - The function `check_for_existing_resources` is called to search the `order` (a `BTreeMap` of memory blocks sorted by size) for the smallest block that is large enough to satisfy the request.
    ///     - If a suitable block is found and it meets the recycling threshold, the block is reused by moving it from the reserve to the in-use list.
    /// 
    /// 2. **Reuse Memory**:
    ///     - If an existing block is found and deemed suitable, the `reuse_pool_memory` function is called.
    ///     - This function moves the block from the reserve to the in-use list, updates the internal accounting, 
    /// and returns a `ResourceResponse` indicating that the memory has been allocated successfully.
    /// 
    /// 3. **Allocate Fresh Memory**:
    ///     - If no suitable block is found in the reserve, the algorithm checks whether the pool has enough 
    ///       capacity to allocate fresh memory.
    ///     - If the current size of the pool (sum of memory in use and in reserve) plus the requested buffer size is within 
    ///       the `max_capacity`, a new memory block is allocated.
    ///     - The allocation is done using the `allocate_fresh_memory` function, which directly allocates memory using 
    ///       the system allocator, updates the internal accounting, and returns a `ResourceResponse` with the allocated memory.
    /// 
    /// 4. **Handle Out of Memory**:
    ///     - If the pool does not have enough capacity to allocate fresh memory, the algorithm returns a `ResourcePoolError::ResourcePoolOutOfMemory` error.
    ///     - In this case, the request is stored in the `wait_queue` (a `BTreeMap` of waiting requests) 
    ///       to be fulfilled later when memory becomes available, potentially through the reclaim process.
    /// 
    /// 5. **Result**:
    ///     - The result of the allocation attempt is either a successful allocation (where memory is
    ///       returned to the caller) or an indication that the pool is out of memory, in which 
    ///       case the request is queued for future fulfillment.
    /// 
    /// ## Complexity:
    /// - **Check for Existing Resources**: The search for an existing block in the reserve uses the 
    ///   logarithmic search properties of `BTreeMap`, making this step `O(log n)`.
    /// - **Reuse Memory**: Moving a block from the reserve to the in-use list involves updating 
    ///   a few data structures, typically `O(1)` operations.
    /// - **Allocate Fresh Memory**: Allocating fresh memory is dependent on the system allocator
    ///     and is generally `O(1)` but could vary based on the underlying system implementation.
    /// - **Overall Complexity**: The overall complexity is dominated by the logarithmic 
    ///   search for existing resources, making the typical case `O(log n)`.
    /// 
    /// # Example Scenarios
    ///
    /// ## Exact match
    ///  The requested buffer size exactly matches a block available in the reserve.
    ///  - **Requested Buffer Size** : 512 bytes
    ///  - **Existing Blocks in Reserve**: 
    ///       - Block A: 512 bytes
    ///       - Block B: 1024 bytes
    ///  - **Recycling Threshold**: 0.8 (80%)
    /// 
    /// ### Process:
    /// - The algorithm finds Block A (512 bytes) in the reserve. Since Block A is an exact match (512/512 = 1.0), 
    ///   it meets the threshold. Block A is moved to the in-use list, fulfilling the request.
    ///
    /// ## Below Threshold (Order not filled)
    ///  The requested buffer size is smaller than a block in reserve but meets the recycling threshold.
    ///  - **Requested Buffer Size**: 800 bytes
    ///  - **Existing Blocks in Reserve**:
    ///       - Block A: 512 bytes
    ///       - Block B: 1024 bytes
    ///  - **Recycling Threshold**: 0.8 (80%)
    /// 
    /// ### Process:
    /// - The algorithm checks the `order` for blocks that can satisfy the request.
    /// - Block B (1024 bytes) is found. The proportion full is calculated as 800/1024 = 0.78125.
    /// - Since 0.78125 is **below** the threshold of 0.8, Block B **cannot** be reused.
    /// 
    /// ### Result:
    /// - The request for 800 bytes remains unfulfilled. The algorithm may attempt to allocate fresh memory or place the request in a waiting queue.
    ///
    /// ## Example Scenario: Below Threshold
    ///  The requested buffer size is too small compared to a block in reserve and does not meet the recycling threshold.
    ///  - **Requested Buffer Size**: 400 bytes
    ///  - **Existing Blocks in Reserve**:
    ///       - Block A: 512 bytes
    ///       - Block B: 1024 bytes
    ///  - **Recycling Threshold**: 0.8 (80%)
    /// 
    /// ### Process:
    /// - The algorithm checks the `order` for blocks that can satisfy the request.
    /// - Block B (1024 bytes) is found. The proportion full is calculated as 400/1024 = 0.390625.
    /// - Since 0.390625 is **well below** the threshold of 0.8, Block B **cannot** be reused.
    /// 
    /// ### Result:
    /// - The request for 400 bytes remains unfulfilled. The algorithm may attempt to 
    /// allocate fresh memory or place the request in a waiting queue.
    ///
    /// ## Example Scenario: Request Too Large
    ///  The requested buffer size is larger than any available block in reserve.
    ///  - **Requested Buffer Size**: 800 bytes
    ///  - **Existing Blocks in Reserve**:
    ///       - Block A: 512 bytes
    ///       - Block B: 600 bytes
    ///  - **Recycling Threshold**: 0.8 (80%)
    /// 
    /// ### Process:
    /// - The algorithm checks the `order` for blocks that can satisfy the request.
    /// - No blocks in reserve are large enough to satisfy the request of 800 bytes.
    /// 
    /// ### Result:
    /// - The request for 800 bytes remains unfulfilled. The algorithm may attempt to allocate 
    ///   fresh memory or place the request in a waiting queue.
    ///
    pub fn allocate(&mut self, requested_buffer_size: usize) -> Result<Arc<ResourceResponse>, ResourcePoolError> {
        // Check the reserve map for a suitable block.
        // returns none if block is not found or if found block is too large
        // for the requested buffer size (to prevent situation where we reuse a block of 
        // 1Mb for 1Kb)
        let resource_request = ResourceRequest::new(requested_buffer_size);

        trace!("\n");
        trace!("A request was received: {:?}", resource_request);
        trace!("Looking for suitable resource to reuse");
        let found: Option<Resource> = self.check_for_existing_resources(resource_request.clone());

        match found {
            Some(resource) => {
                // a suitable resource was found.
                trace!("A Suitable resource was found. Reusing memory pool for allocating the following request: {:?}", resource_request);
                self.reuse_pool_memory(resource)
            }
            None => {
                trace!("A suitable resource does not already exist. Checking to see if we can allocate fresh memory if whether we need to wait");
                trace!("current pool size: {}", self.current_size());
                trace!("Resource request size: {:?}", resource_request);
                trace!("Pool, current state: {:?}", self);

                let fresh_memory_alloc_result = self.allocate_fresh_memory(resource_request);
                match fresh_memory_alloc_result {
                    Ok(sucessful_allocation) => {
                        // fresh memory was allocated, return it.
                        Ok(sucessful_allocation)
                    }
                    Err(err) => match err {
                        ResourcePoolError::ResourcePoolOutOfMemory(resource_request) => {
                            // Out of memory. Store the request in the waiting_requests map.
                            // The reclaiming algorithm deals with filling these requests when
                            // resources become available again.
                            trace!("Pool is full, waiting for resources to become available {:?}", resource_request);
                            self.amount_in_waiting += resource_request.size;
                            self.send_to_server();
                            let resource = Resource::new(ptr::null_mut(), resource_request.size);
                            let resource_response = Arc::new(ResourceResponse::new_waiting(resource));
                            self.wait_queue.insert(resource_request, resource_response.clone());
                            Ok(resource_response.clone())
                        }
                        _ => {
                            Result::Err(err)
                        }
                    }
                }
            }
        }
    }

    /// Finds the first block that is large enough to satisfy the requested buffer size.
    /// Considers the recycling threshold to determine if the block is suitable.
    /// Returns an Option containing the ResourcePoolKey of the found block, or None if no suitable block is found.
    fn check_for_existing_resources(&self, resource_request: ResourceRequest) -> Option<Resource> {
        // Create a key with the requested size and a null pointer
        let key_to_find = Resource::new(ptr::null_mut(), resource_request.size);

        // Find the first block that is large enough
        let resource_opt: Option<(&Resource, &())> = self.order.range(key_to_find..).next();

        // Check if the found block meets the threshold requirement
        resource_opt.and_then(|(resource, _)| {
            let proportion_full: f32 = resource_request.size as f32 / resource.size as f32;
            if self.recycling_threshold <= proportion_full {
                Some(resource.clone())
            } else {
                None
            }
        })
    }

    fn reuse_pool_memory(&mut self, resource: Resource) -> Result<Arc<ResourceResponse>, ResourcePoolError> {
        // move from "reserve" to "in-use"  
        let memory_block: Option<(*mut u64, Vec<u8>)> = self.reserve.remove(&resource.ptr);

        let (ptr, buf) = match memory_block {
            None => {
                // For some reason the buffer that has been found
                // is not actually in reserve 
                return Result::Err(ResourcePoolError::ResourcesFoundButUnexpectedlyUnavailable);
            }
            Some(val) => { (val.0 /*ptr*/, val.1 /*buffer*/) }
        };

        // and also remove it from the size order, it should not be findable for allocation again
        let order_old_value = self.order.remove(&resource);
        match order_old_value {
            None => {
                // old value was already in the ordering map, which should not have been the case
                return Result::Err(ResourcePoolError::UnallocatedMemoryUnexpectedlyInOrderingMap)
            }
            Some(_) => {
                // ptr to memory block was found in the order map, as expected.
                // We do not need to do anything more with this
            }
        }

        // now insert into the "in_use" map.
        // insert returns the old value if it were present, which should never happen
        let old_value: Option<Vec<u8>> = self.in_use.insert(ptr, buf.clone());
        match old_value {
            None => { /*Good, insert was successful*/ }
            Some(_) => {
                // old value was returned from the in_use tree, which means the block 
                // of memory just allocated is already in use!
                return Result::Err(ResourcePoolError::AllocatedBlockAlreadyInUse);
            }
        }

        // Update accounting metrics. 
        // remember that the requested buffer size may be smaller than the actual 
        // block that is reused. The threshold for how much smaller we tolerate is
        // defined by the threshold parameter
        self.amount_in_use += resource.size;
        self.amount_in_reserve -= resource.size;

        let mut resp = ResourceResponse::new_filled(resource);
        resp.resource.ptr = ptr;
        // trace!("Pool state: {:?}", self);
        self.send_to_server();

        return Result::Ok(Arc::new(resp));
    }

    fn allocate_fresh_memory(&mut self, resource_request: ResourceRequest) -> Result<Arc<ResourceResponse>, ResourcePoolError> {
        // we can still allocate new memory because we are within the pool's limits.
        // Allocate new memory if no suitable block is found in the reserve map

        if self.current_size() + resource_request.size <= self.max_capacity {
            trace!("Allocating fresh memory");
            let layout = Layout::from_size_align(resource_request.size, std::mem::align_of::<u8>())
                .expect("Failed to create Layout of u8");
            unsafe {
                // allocate our raw buffer
                let ptr = alloc(layout);
                if !ptr.is_null() {
                    let block = vec![0u8; resource_request.size];
                    self.amount_in_use += resource_request.size;
                    self.in_use.insert(ptr.clone() as *mut u64, block);
                    let resource = Resource::new(ptr as *mut u64, resource_request.size);
                    self.send_to_server();
                    Result::Ok(Arc::new(ResourceResponse::new_filled(resource)))
                } else {
                    // trace!("Pool state: {:?}", self);
                    Result::Err(ResourcePoolError::AllocationFailed)
                }
            }
        } else {
            trace!("Pool state: {:?}", self);
            trace!("Tried to allocate fresh memory but resource pool is full");
            Result::Err(ResourcePoolError::ResourcePoolOutOfMemory(resource_request))
        }
    }
}



impl Observable for ResourcePool {
    fn url(&self) -> Url {
        self.build_url_from_str("/resource_pool")
    }
}



unsafe impl Send for ResourcePool {}

unsafe impl Sync for ResourcePool {}

impl YamlSerializable for ResourcePool {}
impl JsonSerializable for ResourcePool {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::{self, size_of, transmute_copy};
    use std::ptr;
    use log::{info, trace, LevelFilter::Trace};
    use std::thread;
    use std::time::Duration;
    use crate::init_logger::init_logger;

    #[derive(derive_more::Debug)]
    struct AStruct {
        a: u32,
        b: u16,
    }

    impl AStruct {
        pub fn new(a: u32, b: u16) -> Self {
            Self {
                a,
                b,
            }
        }
    }

    #[derive(derive_more::Debug)]
    struct ContainerStruct {
        a_ptr: *mut AStruct,
    }

    impl ContainerStruct {
        fn new(a_ptr: *mut AStruct) -> Self {
            ContainerStruct { a_ptr }
        }

        fn get_astruct_mut(&mut self) -> &mut AStruct {
            unsafe { &mut *self.a_ptr }
        }
    }

    impl Drop for ContainerStruct {
        fn drop(&mut self) {}
    }

    unsafe impl Send for ContainerStruct {}


    #[derive(derive_more::Debug)]
    struct ContainerStructVec {
        a_ptrs: Vec<*mut AStruct>, // Vector of pointers to AStruct
    }

    impl ContainerStructVec {
        fn new(a_ptrs: Vec<*mut AStruct>) -> Self {
            ContainerStructVec { a_ptrs }
        }

        fn get_astruct_mut(&mut self, index: usize) -> Option<&mut AStruct> {
            if let Some(&ptr) = self.a_ptrs.get(index) {
                unsafe { Some(&mut *ptr) }
            } else {
                None
            }
        }
    }

    impl Drop for ContainerStructVec {
        fn drop(&mut self) {}
    }

    // can we store the raw pointer in a NotNull or likelike which is nullable. 
    // we can check if its a null ptr before trying to do stuff with it


    #[test]
    fn test_allocate_and_release() {
        init_logger(Trace);
        let mut pool = ResourcePool::new(1024 * 100); // 100 KB max capacity

        // Allocate 10 KB
        let size = 10 * 1024;
        if let Ok(response) = pool.allocate(size) {
            let ptr = response.resource.ptr;
            // Verify that the memory was allocated
            assert!(!ptr.is_null(), "Failed to allocate memory");

            // Release the memory
            pool.reclaim(ptr);

            // Verify that the memory is in the reserve map
            assert!(pool.reserve.contains_key(&ptr), "Memory not found in reserve map");
        } else {
            panic!("Failed to allocate memory");
        }
    }

    #[test]
    fn test_reuse_reserved_memory() {
        init_logger(Trace);
        let mut pool = ResourcePool::with_threshold(1024 * 100, 0.8); // 100 KB max capacity

        assert_eq!(pool.current_size(), 0, "Pool size should be initially empty");
        assert_eq!(pool.amount_in_use, 0, "Pool size should be initially empty");
        assert_eq!(pool.amount_in_reserve, 0, "Pool size should be initially empty");
        assert!(pool.order.is_empty(), "Pool size should be initially empty");
        assert!(pool.in_use.is_empty(), "Pool size should be initially empty");
        assert!(pool.reserve.is_empty(), "Pool size should be initially empty");

        // Allocate 10 KB
        let size = 10 * 1024;
        if let Ok(response) = pool.allocate(size) {
            let ptr = response.resource.ptr;

            assert_eq!(pool.current_size(), size, "Pool size should equal the amount of data allocated (10240)");
            assert_eq!(pool.amount_in_use, size, "Incorrect amount of data in use");
            assert_eq!(pool.amount_in_reserve, 0, "Data in reserve, expected nothing");
            assert_eq!(pool.order.len(), 0, "The memory is still in use, and so not available in the order tree.");
            assert_eq!(pool.in_use.len(), 1, "There should be one entry in the in-use map");
            assert_eq!(pool.reserve.len(), 0, "Reserve should be empty");

            // Release the memory
            pool.reclaim(ptr);

            assert_eq!(pool.current_size(), size, "The pool size has not changed");
            assert_eq!(pool.amount_in_use, 0, "The amount in use is now 0 as it has been moved to reserve");
            assert_eq!(pool.amount_in_reserve, size, "There is now size data in reserve");
            assert_eq!(pool.order.len(), 1, "There should be one entry in reserve");
            assert_eq!(pool.in_use.len(), 0, "Should be empty");
            assert_eq!(pool.reserve.len(), 1, "Reserve should have 1 entry.");


            // Allocate 9 KB (should reuse the 10 KB block)
            // 0.8 recycling threshold, meaning new buf must be 80% full to be acceptable as reuse.
            let new_size = 9 * 1024;
            if let Ok(new_response) = pool.allocate(new_size) {
                let new_ptr = new_response.resource.ptr;
                // Verify that the memory was reused
                assert_eq!(ptr, new_ptr, "Memory was not reused from the reserve map");

                assert_eq!(pool.current_size(), size, "The pool size has not changed");
                assert_eq!(pool.amount_in_use, size, "The buffer has been reused. It's still 10*1024 bytes long, even though we only use 9*1024");
                assert_eq!(pool.amount_in_reserve, 0, "There is now no data in reserve");
                assert_eq!(pool.order.len(), 0, "The block should be back in use and not in reserve or order");
                assert_eq!(pool.in_use.len(), 1, "Should have 1 entry");
                assert_eq!(pool.reserve.len(), 0, "Reserve should have 0 entry.");
            } else {
                panic!("Failed to allocate memory");
            }

            pool.reclaim(ptr);

            // Check the state of the pool again
            assert_eq!(pool.current_size(), size, "The pool size has not changed");
            assert_eq!(pool.amount_in_use, 0, "The amount in use is now 0 as it has been moved to reserve");
            assert_eq!(pool.amount_in_reserve, size, "There is now size data in reserve");
            assert_eq!(pool.order.len(), 1, "There should be one entry in reserve");
            assert_eq!(pool.in_use.len(), 0, "Should be empty");
            assert_eq!(pool.reserve.len(), 1, "Reserve should have 1 entry.");

            // Allocate a smaller size (6 KB), should allocate fresh memory due to threshold
            let new_size = 6 * 1024;
            if let Ok(new_response) = pool.allocate(new_size) {
                let new_ptr = new_response.resource.ptr;

                // Verify that the memory was newly allocated
                assert_ne!(ptr, new_ptr, "Fresh memory was not allocated");

                assert_eq!(pool.current_size(), size + new_size, "The pool size has changed");
                assert_eq!(pool.amount_in_use, new_size, "The buffer has not been reused. We have allocated new_size bytes and they are in use");
                assert_eq!(pool.amount_in_reserve, size, "The original buffer has not been removed from reserve");
                assert_eq!(pool.order.len(), 1, "The block should size 1");
                assert_eq!(pool.in_use.len(), 1, "Should have 1 entry");
                assert_eq!(pool.reserve.len(), 1, "Reserve should have 1 entry.");
            } else {
                panic!("Failed to allocate memory");
            }
        } else {
            panic!("Failed to allocate memory");
        }
    }

    #[test]
    fn test_allocate__resource_pool_is_empty() {
        init_logger(Trace);
        let max_capacity = 1024; // 1 KB
        let mut pool = ResourcePool::new(max_capacity);

        assert_eq!(pool.max_capacity, 1024);
        assert_eq!(pool.amount_in_use, 0);
        assert_eq!(pool.amount_in_reserve, 0);
        assert_eq!(pool.current_size(), 0);

        if let Ok(response) = pool.allocate(64) {
            let ptr = response.resource.ptr;

            assert!(!ptr.is_null(), "Memory allocation failed");
        }

        // We've allocated 64 bytes
        assert_eq!(pool.current_size(), 64);
        assert_eq!(pool.amount_in_use, 64);
        assert_eq!(pool.amount_in_reserve, 0);
        assert_eq!(pool.max_capacity, 1024);
    }


    /// This test checks that the resource pool works in "plentyful" conditions
    /// I.e. we never need to worry about running out of memory 
    // #[test]
    // fn test_resource_pool_lifecycle() {
    //     init_logger(Trace);
    // 
    //     // 1: Instantiate a fresh ResourcePool with 1024 bytes.
    //     let mut pool = ResourcePool::new(1024); // 1 KB max capacity
    // 
    //     // 2: Allocate 64 bytes and check the metrics.
    //     let size = 64;
    //     let response = pool.allocate(size).expect("Failed to allocate memory");
    //     let ptr = response.resource.ptr;
    //     assert!(!ptr.is_null(), "Memory allocation failed or returned a null pointer");
    // 
    //     // Check that the allocation is properly in the in_use dictionary and is of proper size.
    //     let in_use_block = pool.in_use.get(&ptr).expect("Allocated memory not found in in_use map");
    //     assert_eq!(in_use_block.len(), size, "Allocated block size does not match requested size");
    // 
    //     // 3: Check that reserve is empty and the ordering map does not contain anything.
    //     assert!(pool.reserve.is_empty(), "Reserve map is not empty after allocation");
    //     assert!(pool.order.is_empty(), "Order map is not empty after allocation");
    // 
    //     // 4: Deallocate and check that the memory has been stored in the reserve map.
    //     pool.release(ptr);
    //     assert!(pool.reserve.contains_key(&ptr), "Memory not found in reserve map after release");
    // 
    //     // 5: Verify that the order map contains a pointer with size 64 and has length 1.
    //     assert_eq!(pool.order.len(), 1, "Order map does not have exactly one entry after release");
    //     let order_entry = pool.order.iter().next().expect("Order map is empty");
    //     assert_eq!(order_entry.0.size, size, "Order map entry size does not match the released block size");
    // 
    //     // 6: Allocate another 64 bytes and check that the bytes are reused as expected.
    //     let new_response = pool.allocate(64).expect("Failed to allocate memory");
    //     let new_ptr = new_response.resource.ptr;
    //     assert_eq!(ptr, new_ptr, "Memory block was not reused as expected");
    // 
    //     // 7: Reclaim and check the state of the pool again.
    //     pool.release(new_ptr);
    //     assert_eq!(pool.reserve.len(), 1, "Reserve map does not contain exactly one entry after reclaim");
    //     assert_eq!(pool.order.len(), 1, "Order map does not contain exactly one entry after reclaim");
    // 
    //     // 8: Allocate 56 bytes, check that it reuses the block (threshold should allow reuse).
    //     let smaller_response = pool.allocate(56).expect("Failed to allocate memory");
    //     let smaller_ptr = smaller_response.resource.ptr;
    //     assert_eq!(ptr, smaller_ptr, "Memory block was not reused as expected, despite meeting the threshold");
    // 
    //     // 9: Deallocate and check the state of the resource pool.
    //     pool.release(smaller_ptr);
    //     assert_eq!(pool.reserve.len(), 1, "Reserve map does not contain exactly one entry after releasing smaller block");
    //     assert_eq!(pool.order.len(), 1, "Order map does not contain exactly one entry after releasing smaller block");
    // 
    //     // 10: Allocate 8 bytes, should allocate new memory since it doesn't meet the threshold.
    //     let tiny_response = pool.allocate(8).expect("Failed to allocate memory");
    //     let tiny_ptr = tiny_response.resource.ptr;
    //     assert_ne!(tiny_ptr, ptr, "Memory block was incorrectly reused despite not meeting the threshold");
    // 
    //     // Check that the current capacity reflects the extra 8 bytes.
    //     assert_eq!(pool.pool_size, 72, "Pool current capacity does not reflect the extra 8 bytes allocation");
    // 
    //     // 11: Deallocate and check the state of the pool.
    //     pool.release(tiny_ptr);
    //     assert!(pool.reserve.contains_key(&tiny_ptr), "Memory not found in reserve map after releasing tiny block");
    //     assert_eq!(pool.reserve.len(), 2, "Reserve map does not contain exactly two entries after releasing tiny block");
    //     assert_eq!(pool.order.len(), 2, "Order map does not contain exactly two entries after releasing tiny block");
    // 
    //     // 12: Allocate 1024 bytes, should allocate fresh memory.
    //     let large_response = pool.allocate(1024).expect("Failed to allocate memory");
    //     let large_ptr = large_response.resource.ptr;
    //     assert!(!large_ptr.is_null(), "Memory allocation for 1024 bytes failed or returned a null pointer");
    //     assert_ne!(large_ptr, ptr, "Memory block was incorrectly reused for large allocation");
    //     assert_ne!(large_ptr, tiny_ptr, "Memory block was incorrectly reused for large allocation");
    // 
    //     // Ensure the capacity is maxed out after this allocation.
    //     assert_eq!(pool.pool_size, 1024, "Pool current capacity does not reflect the full allocation");
    // }
    #[test]
    fn test_find_first_sufficient_block_exists() {
        let mut pool = ResourcePool::with_threshold(1024, 0.75);

        // Insert some blocks into the order map
        pool.order.insert(Resource::new(ptr::null_mut(), 64), ());
        pool.order.insert(Resource::new(ptr::null_mut(), 128), ());
        pool.order.insert(Resource::new(ptr::null_mut(), 256), ());

        // Find a block that can accommodate 48 bytes
        let result = pool.check_for_existing_resources(ResourceRequest::new(48));
        assert!(result.is_some(), "Expected to find a suitable block, but found none.");
        assert_eq!(result.unwrap().size, 64, "Expected to find the block of size 64 bytes.");

        // Find a block that can accommodate 80 bytes and meets threshold requirements
        let result = pool.check_for_existing_resources(ResourceRequest::new(80));
        assert!(result.is_none(), "Expected not to find a suitable block, but found one.");

        // Find a block that can accommodate 100 bytes and meets threshold requirements
        let result = pool.check_for_existing_resources(ResourceRequest::new(100));
        assert!(result.is_some(), "Expected to find a suitable block, but found none.");
        assert_eq!(result.unwrap().size, 128, "Expected to find the block of size 128 bytes.");
    }

    #[test]
    fn test_find_first_sufficient_block_no_suitable_block() {
        let mut pool = ResourcePool::new(1024);

        // Insert some blocks into the order map
        pool.order.insert(Resource::new(ptr::null_mut(), 64), ());
        pool.order.insert(Resource::new(ptr::null_mut(), 128), ());
        pool.order.insert(Resource::new(ptr::null_mut(), 256), ());

        // Try to find a block that can accommodate 300 bytes
        let result = pool.check_for_existing_resources(ResourceRequest::new(300));
        assert!(result.is_none(), "Expected to find no suitable block, but found one.");
    }

    #[test]
    fn test_find_first_sufficient_block_exact_match() {
        let mut pool = ResourcePool::new(1024);

        // Insert some blocks into the order map
        pool.order.insert(Resource::new(ptr::null_mut(), 64), ());
        pool.order.insert(Resource::new(ptr::null_mut(), 128), ());
        pool.order.insert(Resource::new(ptr::null_mut(), 256), ());

        // Find a block that exactly matches 128 bytes
        let result = pool.check_for_existing_resources(ResourceRequest::new(128));
        assert!(result.is_some(), "Expected to find a suitable block, but found none.");
        assert_eq!(result.unwrap().size, 128, "Expected to find the block of size 128 bytes.");
    }

    #[test]
    fn test_find_first_sufficient_block_empty_pool() {
        let pool = ResourcePool::new(1024);

        // Try to find a block in an empty pool
        let result = pool.check_for_existing_resources(ResourceRequest::new(128));
        assert!(result.is_none(), "Expected to find no suitable block, but found one.");
    }

    #[test]
    fn test_resource_pool_allocation_and_waiting() {
        init_logger(Trace);
        let max_capacity = 1024;
        let mut pool = ResourcePool::new(max_capacity);

        // 1: Allocate 1024 bytes
        let first_allocation = pool.allocate(1024).expect("First allocation failed");

        // 2: Start a new thread to allocate another 1024 bytes
        let pool_clone = Arc::new(Mutex::new(pool));
        let second_allocation_handle = {
            let pool_clone = Arc::clone(&pool_clone);
            thread::spawn(move || {
                trace!("locking pool.");
                let mut pool = pool_clone.lock();
                let second_allocation: Arc<ResourceResponse> = pool.allocate(1024).expect("Second allocation failed");
                drop(pool);
                trace!("pool dropped");

                // Wait until the resource becomes available
                trace!("Locking second_allocation (type: ResourceResponse)");
                second_allocation.wait_until_filled();
                second_allocation
            })
        };

        // 3: Wait for 2 seconds to simulate some work
        thread::sleep(Duration::from_secs(2));

        // 4: Release the first allocation, freeing 1024 bytes

        let mut pool_guard = pool_clone.lock();
        let first_ptr = first_allocation.resource.ptr;
        // drop(pool_guard);
        pool_guard.reclaim(first_ptr);

        // 5: Wait for the second allocation to complete
        let second_allocation = second_allocation_handle.join().expect("Thread panicked");


        // Check that the first allocation was filled immediately
        let first_state = first_allocation.state.lock();
        assert_eq!(*first_state, ResourceRequestState::Filled);

        // Check that the second allocation was waiting initially and then filled
        let second_state = second_allocation.state.lock();
        assert_eq!(*second_state, ResourceRequestState::Filled);
    }
}


#[cfg(test)]
mod resource_pool_key_tests {
    use super::*;
    use std::collections::BTreeMap;
    use log::LevelFilter::Trace;
    use crate::init_logger::init_logger;

    #[test]
    fn test_insertion_and_retrieval_by_size() {
        let mut map: BTreeMap<Resource, String> = BTreeMap::new();

        // Insert elements with different sizes
        let key1 = Resource::new(ptr::null_mut(), 32);
        let key2 = Resource::new(ptr::null_mut(), 64);
        let key3 = Resource::new(ptr::null_mut(), 16);

        map.insert(key1.clone(), "Block 32".to_string());
        map.insert(key2.clone(), "Block 64".to_string());
        map.insert(key3.clone(), "Block 16".to_string());

        // Retrieve by size (find the entry by constructing a ResourcePoolKey with the same size)
        let search_key = Resource::new(ptr::null_mut(), 32);
        assert_eq!(map.get(&search_key), Some(&"Block 32".to_string()));

        let search_key = Resource::new(ptr::null_mut(), 64);
        assert_eq!(map.get(&search_key), Some(&"Block 64".to_string()));

        let search_key = Resource::new(ptr::null_mut(), 16);
        assert_eq!(map.get(&search_key), Some(&"Block 16".to_string()));
    }

    #[test]
    fn test_sorted_order_by_size() {
        let mut map: BTreeMap<Resource, String> = BTreeMap::new();

        // Insert elements with different sizes
        let key1 = Resource::new(ptr::null_mut(), 32);
        let key2 = Resource::new(ptr::null_mut(), 64);
        let key3 = Resource::new(ptr::null_mut(), 16);

        map.insert(key1.clone(), "Block 32".to_string());
        map.insert(key2.clone(), "Block 64".to_string());
        map.insert(key3.clone(), "Block 16".to_string());

        // Check that the keys are stored in sorted order
        let mut iter = map.keys();

        assert_eq!(iter.next(), Some(&Resource::new(ptr::null_mut(), 16)));
        assert_eq!(iter.next(), Some(&Resource::new(ptr::null_mut(), 32)));
        assert_eq!(iter.next(), Some(&Resource::new(ptr::null_mut(), 64)));
    }

    #[test]
    fn test_handling_of_equal_sizes_different_pointers() {
        init_logger(Trace);
        let mut map: BTreeMap<Resource, String> = BTreeMap::new();

        // Insert elements with the same size but different pointers
        let ptr1: *mut u64 = 0x1 as *mut u64;
        let ptr2: *mut u64 = 0x2 as *mut u64;

        let key1 = Resource::new(ptr1, 32);
        let key2 = Resource::new(ptr2, 32);

        trace!("key1: {:?}", key1.ptr);
        trace!("key2: {:?}", key2.ptr);

        map.insert(key1.clone(), "Block 1".to_string());
        map.insert(key2.clone(), "Block 2".to_string());

        assert_eq!(map.len(), 2, "Map is not size 2");

        // trace!("map: {:?}", map);
        // for (k,v) in map.iter() {
        //     trace!("k: {:?}, v: {:?}", k, v);
        // }
        let mut iter = map.iter();

        let (k1, v1) = iter.next().unwrap();
        let (k2, v2) = iter.next().unwrap();

        trace!("k1: {:?}", k1);

        assert_ne!(k1.ptr, k2.ptr);
        assert_eq!(k1.size, k2.size);

        assert_eq!(v1, "Block 1");
        assert_eq!(v2, "Block 2");
    }
}


#[cfg(test)]
mod test_accounting_metrics {
    use super::*;
    use std::ptr;
    use std::sync::Arc;
    use log::LevelFilter::Trace;
    use crate::init_logger::init_logger;

    #[test]
    fn test_initial_pool_state() {
        let pool = ResourcePool::new(1024);

        assert_eq!(pool.current_size(), 0, "Initial pool size should be 0");
        assert_eq!(pool.amount_in_use, 0, "Initial amount in use should be 0");
        assert_eq!(pool.amount_in_reserve, 0, "Initial amount in reserve should be 0");
        assert!(pool.in_use.is_empty(), "In-use map should be empty");
        assert!(pool.reserve.is_empty(), "Reserve map should be empty");
        assert!(pool.order.is_empty(), "Order map should be empty");
    }

    #[test]
    fn test_allocate_memory() {
        let mut pool = ResourcePool::new(1024);

        // Allocate 64 bytes
        let response = pool.allocate(64).expect("Failed to allocate memory");
        assert_eq!(pool.current_size(), 64, "Pool size should be 64 after allocation");
        assert_eq!(pool.amount_in_use, 64, "Amount in use should be 64 after allocation");
        assert_eq!(pool.amount_in_reserve, 0, "Amount in reserve should be 0 after allocation");
        assert_eq!(pool.in_use.len(), 1, "In-use map should contain 1 entry");
        assert!(pool.reserve.is_empty(), "Reserve map should be empty after allocation");
        assert!(pool.order.is_empty(), "Order map should be empty after allocation");

        // Reclaim memory
        pool.reclaim(response.resource.ptr);
        assert_eq!(pool.amount_in_use, 0, "Amount in use should be 0 after reclaim");
        assert_eq!(pool.amount_in_reserve, 64, "Amount in reserve should be 64 after reclaim");
        assert!(pool.in_use.is_empty(), "In-use map should be empty after reclaim");
        assert_eq!(pool.reserve.len(), 1, "Reserve map should contain 1 entry");
        assert_eq!(pool.order.len(), 1, "Order map should contain 1 entry");
    }

    #[test]
    fn test_reuse_memory_from_reserve() {
        let mut pool = ResourcePool::new(1024);

        // Allocate and reclaim memory
        let response = pool.allocate(64).expect("Failed to allocate memory");
        let ptr = response.resource.ptr;
        pool.reclaim(ptr);

        // Allocate again, should reuse memory from reserve
        let response2 = pool.allocate(64).expect("Failed to allocate memory");
        assert_eq!(ptr, response2.resource.ptr, "Memory should be reused from reserve");

        assert_eq!(pool.current_size(), 64, "Pool size should remain 64");
        assert_eq!(pool.amount_in_use, 64, "Amount in use should be 64 after re-allocation");
        assert_eq!(pool.amount_in_reserve, 0, "Amount in reserve should be 0 after re-allocation");
        assert_eq!(pool.in_use.len(), 1, "In-use map should contain 1 entry");
        assert!(pool.reserve.is_empty(), "Reserve map should be empty after re-allocation");
        assert!(pool.order.is_empty(), "Order map should be empty after re-allocation");
    }

    #[test]
    fn test_allocate_more_than_pool_capacity() {
        init_logger(Trace);
        let mut pool = ResourcePool::new(1024);

        // Allocate more than the pool's capacity
        let result = pool.allocate(2048);
        trace!("result: {:?}", result);
        // assert!(result.is_err(), "Allocation should fail when exceeding pool capacity");
        assert_eq!(pool.current_size(), 0, "Pool size should remain 0 after failed allocation");
        assert_eq!(pool.amount_in_use, 0, "Amount in use should remain 0 after failed allocation");
        assert_eq!(pool.amount_in_reserve, 0, "Amount in reserve should remain 0 after failed allocation");
    }

    #[test]
    fn test_allocate_below_threshold() {
        let mut pool = ResourcePool::with_threshold(1024, 0.8);

        // Allocate 64 bytes and reclaim it
        let response = pool.allocate(64).expect("Failed to allocate memory");
        let ptr = response.resource.ptr;
        pool.reclaim(ptr);

        // Allocate 32 bytes, should allocate fresh memory due to threshold
        let response2 = pool.allocate(32).expect("Failed to allocate memory");
        assert_ne!(ptr, response2.resource.ptr, "Memory should not be reused due to threshold");

        assert_eq!(pool.current_size(), 96, "Pool size should be 96 after new allocation");
        assert_eq!(pool.amount_in_use, 32, "Amount in use should be 32 after new allocation");
        assert_eq!(pool.amount_in_reserve, 64, "Amount in reserve should remain 64");
        assert_eq!(pool.in_use.len(), 1, "In-use map should contain 1 entry");
        assert_eq!(pool.reserve.len(), 1, "Reserve map should contain 1 entry");
        assert_eq!(pool.order.len(), 1, "Order map should contain 1 entry");
    }

    #[test]
    fn test_allocate_fresh_memory_when_none_in_reserve() {
        let mut pool = ResourcePool::new(1024);

        // Allocate 64 bytes
        let response = pool.allocate(64).expect("Failed to allocate memory");
        let ptr = response.resource.ptr;

        // Allocate another 64 bytes
        let response2 = pool.allocate(64).expect("Failed to allocate memory");
        assert_ne!(ptr, response2.resource.ptr, "Memory should not be reused");

        assert_eq!(pool.current_size(), 128, "Pool size should be 128 after new allocation");
        assert_eq!(pool.amount_in_use, 128, "Amount in use should be 128 after new allocation");
        assert_eq!(pool.amount_in_reserve, 0, "Amount in reserve should be 0 after new allocation");
        assert_eq!(pool.in_use.len(), 2, "In-use map should contain 2 entries");
        assert!(pool.reserve.is_empty(), "Reserve map should be empty after new allocation");
        assert!(pool.order.is_empty(), "Order map should be empty after new allocation");
    }
}

#[cfg(test)]
mod reclaim_tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use log::LevelFilter::Trace;
    use crate::init_logger::init_logger;

    fn add_block_to_pool(pool: &mut ResourcePool, size: usize) -> *mut u64 {
        let block = vec![0u8; size];
        let ptr = block.as_ptr() as *mut u64;
        pool.in_use.insert(ptr, block);
        pool.amount_in_use += size;
        ptr
    }

    fn move_block_to_reserve(pool: &mut ResourcePool, ptr: *mut u64, size: usize) {
        if let Some((_ptr_duplicate, block)) = pool.in_use.remove(&ptr) {
            trace!("ptr: {:?}", ptr);
            trace!("block : {:?}", block);
            pool.reserve.insert(ptr, block);
            pool.amount_in_reserve += size;
            pool.amount_in_use -= size;
        }
    }

    /// Checks the behavior of the `reclaim` function in the `ResourcePool`.
    ///
    /// This function tests the reclaim process in a `ResourcePool` with a specific configuration.
    /// It verifies that when a block of memory is reclaimed, it is correctly reassigned to a
    /// waiting request if an exact match or a match within the recycling threshold is found.
    ///
    /// # Parameters
    ///
    /// - `pool_capacity`: The total size of the memory pool, representing the maximum memory that can be managed by the pool.
    /// - `threshold`: The minimum proportion of the reclaimed block that must be used by a request for it to be considered a good match.
    /// - `block_size`: The size of the block that will be added to the pool and later reclaimed. This block simulates the memory available for reuse.
    /// - `request_size`: The size of the resource request that the pool should attempt to fulfill during the reclaim process. It simulates a waiting request that matches the block size.
    ///
    /// # Usage
    ///
    /// This function is useful for testing various scenarios where the `reclaim` function is expected to
    /// assign memory blocks to waiting requests. You can call this function in different test cases by providing
    /// specific values for `pool_capacity`, `threshold`, `block_size`, and `request_size`.
    ///
    /// This example tests that a block of size 512 bytes is correctly reclaimed and reassigned to a
    /// waiting request of the same size, resulting in 512 bytes in use and no bytes left in reserve.
    fn check_reclaim(
        pool_capacity: usize,
        threshold: f32,
        block_size: usize,
        request_size: usize,
    ) {
        init_logger(Trace);
        let mut pool = ResourcePool::with_threshold(pool_capacity, threshold);
        let ptr = add_block_to_pool(&mut pool, block_size);

        // Insert a waiting request with exact match
        let resource_request = ResourceRequest::new(request_size);
        let resource_response = Arc::new(ResourceResponse::new_waiting(Resource::new(ptr::null_mut(), request_size)));
        pool.wait_queue.insert(resource_request.clone(), resource_response.clone());
        pool.amount_in_waiting += request_size;

        // Calculate the expected values
        let proportion_full: f32 = request_size as f32 / block_size as f32;
        let block_should_be_reassigned: bool = threshold <= proportion_full && proportion_full <= 1.0;
        let (expected_in_use, expected_in_reserve) = if block_should_be_reassigned {
            (block_size, 0) // Block is assigned to the request
        } else {
            (0, block_size) // Block remains in reserve, request not fulfilled
        };

        // Reclaim the block
        pool.reclaim(ptr);

        // Check that the request has been fulfilled or not based on the calculated values
        if block_should_be_reassigned {
            assert_eq!(*resource_response.state.lock(), ResourceRequestState::Filled);
            assert_eq!(resource_response.get_resource_ptr(), ptr);
        } else {
            assert_eq!(*resource_response.state.lock(), ResourceRequestState::Waiting);
            // not yet assigned!
            assert_eq!(resource_response.get_resource_ptr(), std::ptr::null_mut());
        }

        assert_eq!(pool.amount_in_reserve, expected_in_reserve);
        assert_eq!(pool.amount_in_use, expected_in_use);
    }


    #[test]
    fn test_reclaim_no_request_waiting() {
        init_logger(Trace);
        let mut pool = ResourcePool::new(1024);
        let size = 512;
        let ptr = add_block_to_pool(&mut pool, size);

        // Reclaim the block
        pool.reclaim(ptr);

        // Check that the block is moved to reserve
        assert!(pool.reserve.contains_key(&ptr));
        assert!(pool.in_use.is_empty());
        assert_eq!(pool.amount_in_reserve, size);
        assert_eq!(pool.amount_in_use, 0);
    }

    #[test]
    fn test_reclaim_request_with_exact_match_scenario() {
        let pool_size = 1024;
        let threshold = 0.8;
        let block_size = 512;
        let request_size = 512;
        check_reclaim(
            pool_size,
            threshold,
            block_size,
            request_size,
        );
    }

    #[test]
    fn test_reclaim_request_with_exact_match2() {
        /*
        pool size is 1024, we allocate 1024 bytes which get reclaimed. Meanwhile there 
        is another requestfor 1024 bytes in waiting. The byte buffer is reused, exactl match
         */
        let pool_size = 1024;
        let pool_threshold = 0.8;
        let initial_block_size_that_gets_reclaimed = 1024;
        let waiting_request_size = 1024;
        check_reclaim(
            pool_size,
            pool_threshold,
            initial_block_size_that_gets_reclaimed,
            waiting_request_size,
        );
    }

    #[test]
    fn test_reclaim_request_above_threshold() {
        /*
        This time our pool is also 1024 bytes with a 0.8 threshold. 
        We request 512 bytes and they get reclaimed. Meanwhile we have 
        another request waiting in the queue for an amount that will 
        be accepted within threshold. Thats ceil(512 * 0.8) = 410. 
        The waiting request size needs to be greater than 410 but less than 512
        
         */
        let pool_size = 1024;
        let threshold = 0.8;
        let block_size = 512;  // * 0.8 = 409.6
        let request_size = 420;

        check_reclaim(
            pool_size,
            threshold,
            block_size,
            request_size,
        );
    }

    #[test]
    fn test_reclaim_request_under_threshold() {
        /*
        This time our pool is also 1024 bytes with a 0.8 threshold. 
        We request 512 bytes and they get reclaimed. Meanwhile we have 
        another request waiting in the queue for an amount that is not
        accepted within threshold. Thats ceil(512 * 0.8) = 410. 
        The waiting request size needs to be less than 410 
        
         */
        let pool_size = 1024;
        let threshold = 0.8;
        let block_size = 512;  // * 0.8 = 409.6
        let request_size = 400;

        check_reclaim(
            pool_size,
            threshold,
            block_size,
            request_size,
        );
    }

    #[test]
    fn test_reclaim_request_too_big() {
        /*
        This time our pool is also 1024 bytes with a 0.8 threshold. 
        We request 512 bytes and they get reclaimed. Meanwhile we have 
        another request waiting that is too big. In this case, our 
        pool should not reuse the block, since its not big enough 
        for the waiting request.
        
         */
        let pool_size = 1024;
        let threshold = 0.8;
        let block_size = 512;  // * 0.8 = 409.6
        let request_size = 600;

        check_reclaim(
            pool_size,
            threshold,
            block_size,
            request_size,
        );
    }

    #[test]
    fn test_reclaim_with_multiple_waiting_requests() {
        let pool_size = 1024;
        let threshold = 0.8;
        let block_size = 512;

        init_logger(Trace);
        let mut pool = ResourcePool::with_threshold(pool_size, threshold);
        let ptr = add_block_to_pool(&mut pool, block_size);

        // Insert multiple waiting requests
        let request_size_1 = 420; // within threshold
        let request_size_2 = 400; // below threshold

        let resource_request_1 = ResourceRequest::new(request_size_1);
        let resource_response_1 = Arc::new(ResourceResponse::new_waiting(Resource::new(ptr::null_mut(), request_size_1)));
        pool.wait_queue.insert(resource_request_1.clone(), resource_response_1.clone());
        pool.amount_in_waiting += request_size_1;

        let resource_request_2 = ResourceRequest::new(request_size_2);
        let resource_response_2 = Arc::new(ResourceResponse::new_waiting(Resource::new(ptr::null_mut(), request_size_2)));
        pool.wait_queue.insert(resource_request_2.clone(), resource_response_2.clone());
        pool.amount_in_waiting += request_size_2;

        // Reclaim the block
        pool.reclaim(ptr);

        // Check that the correct request has been fulfilled
        assert_eq!(*resource_response_1.state.lock(), ResourceRequestState::Filled);
        assert_eq!(resource_response_1.get_resource_ptr(), ptr);
        assert_eq!(*resource_response_2.state.lock(), ResourceRequestState::Waiting);
        assert!(pool.in_use.contains_key(&ptr));
        assert_eq!(pool.amount_in_reserve, 0);
        assert_eq!(pool.amount_in_use, block_size);
    }

    #[test]
    fn test_reclaim_with_zero_threshold() {
        let pool_size = 1024;
        let threshold = 0.0;
        let block_size = 512;
        let request_size = 10; // much smaller than block size

        check_reclaim(
            pool_size,
            threshold,
            block_size,
            request_size,
        );
    }

    #[test]
    fn test_reclaim_with_exact_threshold_match() {
        let pool_size = 1024;
        let threshold = 0.8;
        let block_size = 512;
        let request_size = (block_size as f32 * threshold).ceil() as usize;

        check_reclaim(
            pool_size,
            threshold,
            block_size,
            request_size,
        );
    }
}


#[cfg(test)]
#[ignore]
mod stress_tests {
    use std::sync::Arc;
    use crossbeam::channel::unbounded;
    use log::LevelFilter::{Trace, Warn};
    use log::trace;
    use parking_lot::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread;
    use std::time::{Duration, Instant};
    use rand::Rng;
    use crate::init_logger::init_logger;
    use crate::observable::Observable;
    use crate::resource_pool::ResourcePool;

    // Function to generate a properly aligned random size
    fn randomly_generate_size(max_variable_power_of_2: usize, max_number_to_allocate: usize) -> usize {
        // Alignment sizes (powers of two)
        let mut alignments = vec![];
        for i in 1..=max_variable_power_of_2 {
            alignments.push(1 << i);
        }

        trace!("alignments: {:?}", alignments);

        // Get a random number generator
        let mut rng = rand::thread_rng();

        // Choose a random alignment size
        let element_size = alignments[rng.gen_range(0..alignments.len())];

        // Choose a random number of elements to allocate, between 1 and max_number_to_allocate
        let num_elements = rng.gen_range(1..=max_number_to_allocate);

        // Total size for allocation
        element_size * num_elements
    }

    fn run_stress_test(run_time: Duration, max_capacity: usize, threshold: f32, max_variable_power_of_2: usize, max_number_to_allocate:usize) {
        let url = "http://127.0.0.1:5000/data";

        // Initialize logging if necessary
        // init_logger(Warn);
        trace!("Starting test");

        let pool = Arc::new(Mutex::new(ResourcePool::with_threshold(max_capacity, threshold)));
        let pool_lock = pool.lock();
        pool_lock.send_to_server();
        // trace!("pool start: {:?}", pool_lock.to_json());
        drop(pool_lock);
        let start_time = Instant::now();

        // Shared atomic boolean to signal threads to stop
        let stop_signal = Arc::new(AtomicBool::new(false));

        // Channel for communication between allocator and consumer threads
        let (tx, rx) = unbounded();

        // Thread 1: Allocator
        let allocator_pool = Arc::clone(&pool);
        let allocator_stop_signal = Arc::clone(&stop_signal);

        let allocator_handle = thread::Builder::new()
            .name("ProducerThread".into())
            .spawn(move || {
                while !allocator_stop_signal.load(Ordering::Relaxed) {
                    let mut pool_guard = allocator_pool.lock();
                    let time_finished = start_time.elapsed() >= run_time;
                    if time_finished && pool_guard.wait_list_empty() {
                        allocator_stop_signal.store(true, Ordering::Relaxed);
                        break;
                    }
                    if !time_finished {
                        // only generate new requests for memory when we have not finished.
                        let size_to_allocate = randomly_generate_size(max_variable_power_of_2, max_number_to_allocate);
                        pool_guard.send_to_server();
                        trace!("Requesting memory: {:?} bytes", size_to_allocate);
                        match pool_guard.allocate(size_to_allocate) {
                            Ok(resource_allocation_response) => {
                                tx.send(resource_allocation_response).expect("Failed to send resource to consumer");
                            }
                            Err(e) => {
                                trace!("Allocation failed: {:?}", e);
                            }
                        }
                    } else {
                        trace!("Time is up, but we are waiting on the queue to be empty before finishing. Size is : {:?}", pool_guard.wait_list_size());
                        trace!("queue: {:?}", pool_guard.wait_queue.keys());
                        trace!("reserve order keys: {:?}", pool_guard.order.keys());
                        trace!("Space left in pool: {:?}", pool_guard)
                    }
                    drop(pool_guard);
                    thread::sleep(Duration::from_millis(10)); // Simulate some delay between allocations
                }
            }).unwrap();

        // Thread 2: Consumer/Processor
        let pool_consumer = Arc::clone(&pool);
        let consumer_stop_signal = Arc::clone(&stop_signal);
        let consumer_handle = thread::Builder::new()
            .name("ConsumerThread".into())
            .spawn(move || {
                while !consumer_stop_signal.load(Ordering::Relaxed) {
                    if let Ok(resource_allocation_request) = rx.recv() {
                        trace!("Reclaiming resource: {:?}", resource_allocation_request);
                        // Reclaim the resource back into the pool
                        let ptr = resource_allocation_request.resource.ptr;
                        let mut pool_guard = pool_consumer.lock();
                        pool_guard.reclaim(ptr);
                        pool_guard.send_to_server();
                        drop(pool_guard);
                    }
                }
            }).unwrap();

        // Wait for both threads to complete
        allocator_handle.join().expect("Allocator thread panicked");
        consumer_handle.join().expect("Consumer thread panicked");

        // After stress test, verify the resource pool state
        let final_pool_state = pool.lock();
        final_pool_state.send_to_server();
        assert!(final_pool_state.amount_in_use == 0, "All allocated memory should be reclaimed");
        assert!(final_pool_state.current_size() <= final_pool_state.max_capacity, "Pool size should not exceed max capacity");
        assert!(final_pool_state.wait_queue.is_empty(), "Resource request queue should be empty");
        assert!(final_pool_state.in_use.is_empty(), "In-use map should be empty after test");
        assert_eq!(final_pool_state.amount_in_reserve, final_pool_state.current_size(), "All memory should be in reserve after test");
        trace!("Final pool state: {:?}", *final_pool_state);
    }

    #[test]
    // #[ignore]
    fn stress_test1() {
        init_logger(Trace);
        let seconds = 2;
        let max_data_size_exponent = 10; /*2**10 = 1024*/
        let max_number_to_allocate = 10; // allow allocating up to 10 item of max_data_size
        let max_capacity = ((1 << max_data_size_exponent) * max_number_to_allocate) * 10; 
        // 2^1 to 2^10
        run_stress_test(Duration::from_secs(seconds), max_capacity, 0., max_data_size_exponent, max_number_to_allocate);
    }
    
    
}
