# Resource Pool

This Rust module provides a mechanism to manage memory efficiently by implementing a resource pool that controls memory usage. The resource pool is designed to limit memory consumption and manage memory allocation and deallocation, ensuring that memory is reused wherever possible. This module was developed as part of a larger project but is currently not used in the main project. It is kept as a reference and might be used later.

## Overview

The resource pool manages memory allocation by either reusing existing memory blocks from a reserve or allocating fresh memory if necessary. It also supports a reclamation mechanism that matches reclaimed memory blocks with waiting requests based on a recycling threshold.

### Key Features

- **Memory Reuse**: Allocated memory blocks that are no longer in use are moved to a reserve and can be reused for future allocations.
- **Reclamation**: Reclaimed memory blocks are matched with waiting requests if they meet a recycling threshold.
- **Threshold-Based Matching**: Blocks are only reused if the size of the request meets a specified percentage (threshold) of the block size.
- **Resource Pool Metrics**: The resource pool tracks various metrics such as the amount of memory in use, in reserve, and the efficiency of memory usage.

## Usage

### Creating a Resource Pool

To create a resource pool with a specified maximum capacity:

```rust
let pool = ResourcePool::new(max_capacity);

// You can also specify a recycling threshold:
let pool = ResourcePool::with_threshold(max_capacity, recycling_threshold);
```

## Allocating Memory
To allocate memory from the resource pool:
```rust 
let size = 1024; // Size in bytes
let response = pool.allocate(size).expect("Failed to allocate memory");
```

If the pool has enough capacity and a suitable block is available in the reserve, the memory is allocated and returned to the caller.

## Reclaiming Memory
When memory is no longer needed, it can be reclaimed:

```rust
let ptr = response.resource.ptr;
pool.reclaim(ptr);
```

Reclaimed memory is either reused immediately or moved to the reserve for future use.

## Example Scenarios

### Exact Match Allocation
If a requested buffer size exactly matches a block available in the reserve, that block will be reused.

### Below Threshold Allocation
If the requested buffer size is smaller than a block in the reserve but does not meet the recycling threshold, the block will not be reused, and fresh memory may be allocated.

## Reclaiming Memory
Reclaimed memory blocks are matched with waiting requests if they meet the recycling threshold. If no suitable request is found, the block is moved to the reserve.

## Conclusion

The resource pool is a robust mechanism for managing memory efficiently in Rust applications. It minimizes memory fragmentation by reusing memory blocks wherever possible and ensures that memory usage stays within specified limits. While this module is not currently used in the main project, it provides a valuable reference for future memory management needs.
