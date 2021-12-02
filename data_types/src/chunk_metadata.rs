//! Module contains a representation of chunk metadata
use std::{convert::TryFrom, num::NonZeroU32, sync::Arc};

use bytes::Bytes;
use snafu::{ResultExt, Snafu};
use time::Time;
use uuid::Uuid;

use crate::partition_metadata::PartitionAddr;

/// Address of the chunk within the catalog
#[derive(Debug, Clone, Eq, PartialEq, PartialOrd, Ord)]
pub struct ChunkAddr {
    /// Database name
    pub db_name: Arc<str>,

    /// What table does the chunk belong to?
    pub table_name: Arc<str>,

    /// What partition does the chunk belong to?
    pub partition_key: Arc<str>,

    /// The ID of the chunk
    pub chunk_id: ChunkId,
}

impl ChunkAddr {
    pub fn new(partition: &PartitionAddr, chunk_id: ChunkId) -> Self {
        Self {
            db_name: Arc::clone(&partition.db_name),
            table_name: Arc::clone(&partition.table_name),
            partition_key: Arc::clone(&partition.partition_key),
            chunk_id,
        }
    }

    pub fn into_partition(self) -> PartitionAddr {
        PartitionAddr {
            db_name: self.db_name,
            table_name: self.table_name,
            partition_key: self.partition_key,
        }
    }
}

impl std::fmt::Display for ChunkAddr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Chunk('{}':'{}':'{}':{})",
            self.db_name,
            self.table_name,
            self.partition_key,
            self.chunk_id.get()
        )
    }
}

/// Which storage system is a chunk located in?
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Clone, Copy)]
pub enum ChunkStorage {
    /// The chunk is still open for new writes, in the Mutable Buffer
    OpenMutableBuffer,

    /// The chunk is no longer open for writes, in the Mutable Buffer
    ClosedMutableBuffer,

    /// The chunk is in the Read Buffer (where it can not be mutated)
    ReadBuffer,

    /// The chunk is both in ReadBuffer and Object Store
    ReadBufferAndObjectStore,

    /// The chunk is stored in Object Storage (where it can not be mutated)
    ObjectStoreOnly,
}

impl ChunkStorage {
    /// Return a str representation of this storage state
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::OpenMutableBuffer => "OpenMutableBuffer",
            Self::ClosedMutableBuffer => "ClosedMutableBuffer",
            Self::ReadBuffer => "ReadBuffer",
            Self::ReadBufferAndObjectStore => "ReadBufferAndObjectStore",
            Self::ObjectStoreOnly => "ObjectStoreOnly",
        }
    }
}

/// Any lifecycle action currently in progress for this chunk
#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum ChunkLifecycleAction {
    /// Chunk is in the process of being written to object storage
    Persisting,

    /// Chunk is in the process of being compacted
    Compacting,

    /// Object Store Chunk is in the process of being compacted
    /// The ChunkId is the ID of the new chunk that will replace this chunk
    /// after the compaction is completed
    CompactingObjectStore(ChunkId),

    /// Chunk is about to be dropped from memory and (if persisted) from object store
    Dropping,

    /// Chunk is in the process of being loaded back into the RUB
    LoadingReadBuffer,
}

impl std::fmt::Display for ChunkLifecycleAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.name())
    }
}

impl ChunkLifecycleAction {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Persisting => "Persisting to Object Storage",
            Self::Compacting => "Compacting",
            Self::CompactingObjectStore(_chunk_id) => "Compacting Object Store",
            Self::Dropping => "Dropping",
            Self::LoadingReadBuffer => "Loading to Read Buffer",
        }
    }
}

/// Represents metadata about the physical storage of a chunk in a
/// database.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Clone)]
pub struct ChunkSummary {
    /// The partition key of this chunk
    pub partition_key: Arc<str>,

    /// The table of this chunk
    pub table_name: Arc<str>,

    /// Order of this chunk relative to other overlapping chunks.
    pub order: ChunkOrder,

    /// The id of this chunk
    pub id: ChunkId,

    /// How is this chunk stored?
    pub storage: ChunkStorage,

    /// Is there any outstanding lifecycle action for this chunk?
    pub lifecycle_action: Option<ChunkLifecycleAction>,

    /// The number of bytes used to store this chunk in memory
    pub memory_bytes: usize,

    /// The number of bytes used to store this chunk in object storage
    pub object_store_bytes: usize,

    /// The total number of rows in this chunk
    pub row_count: usize,

    /// The time at which the chunk data was accessed, by a query or a write
    pub time_of_last_access: Option<Time>,

    /// The earliest time at which data contained within this chunk was written
    /// into IOx. Note due to the compaction, etc... this may not be the chunk
    /// that data was originally written into
    pub time_of_first_write: Time,

    /// The latest time at which data contained within this chunk was written
    /// into IOx. Note due to the compaction, etc... this may not be the chunk
    /// that data was originally written into
    pub time_of_last_write: Time,
}

/// Represents metadata about the physical storage of a column in a chunk
#[derive(Debug, PartialEq, Eq, Clone)]
pub struct ChunkColumnSummary {
    /// Column name
    pub name: Arc<str>,

    /// Estimated size, in bytes, consumed by this column.
    pub memory_bytes: usize,
}

/// Contains additional per-column details about physical storage of a chunk
#[derive(Debug, PartialEq, Eq, Clone)]
pub struct DetailedChunkSummary {
    /// Overall chunk statistic
    pub inner: ChunkSummary,

    /// Per column breakdown
    pub columns: Vec<ChunkColumnSummary>,
}

impl ChunkSummary {
    pub fn equal_without_timestamps_and_ids(&self, other: &Self) -> bool {
        self.partition_key == other.partition_key
            && self.table_name == other.table_name
            && self.storage == other.storage
            && self.lifecycle_action == other.lifecycle_action
            && self.memory_bytes == other.memory_bytes
            && self.object_store_bytes == other.object_store_bytes
            && self.row_count == other.row_count
    }
}

/// ID of a chunk.
///
/// This ID is unique within a single partition.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ChunkId(Uuid);

impl ChunkId {
    /// Create new, random ID.
    #[allow(clippy::new_without_default)] // `new` creates non-deterministic result
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    /// **TESTING ONLY:** Create new ID from integer.
    ///
    /// Since this can easily lead to ID collissions (which in turn can lead to panics), this must only be used for
    /// testing purposes!
    pub fn new_test(id: u128) -> Self {
        Self(Uuid::from_u128(id))
    }

    /// Get inner UUID.
    pub fn get(&self) -> Uuid {
        self.0
    }
}

impl std::fmt::Debug for ChunkId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        <Self as std::fmt::Display>::fmt(self, f)
    }
}

impl std::fmt::Display for ChunkId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if (self.0.get_variant() == Some(uuid::Variant::RFC4122))
            && (self.0.get_version() == Some(uuid::Version::Random))
        {
            f.debug_tuple("ChunkId").field(&self.0).finish()
        } else {
            f.debug_tuple("ChunkId").field(&self.0.as_u128()).finish()
        }
    }
}

impl From<ChunkId> for Bytes {
    fn from(id: ChunkId) -> Self {
        id.get().as_bytes().to_vec().into()
    }
}

#[derive(Debug, Snafu)]
pub enum BytesToChunkIdError {
    #[snafu(display("Cannot convert bytes to chunk ID: {}", source))]
    CannotConvertBytes { source: uuid::Error },
}

impl TryFrom<Bytes> for ChunkId {
    type Error = BytesToChunkIdError;

    fn try_from(value: Bytes) -> Result<Self, Self::Error> {
        Ok(Self(Uuid::from_slice(&value).context(CannotConvertBytes)?))
    }
}

impl From<Uuid> for ChunkId {
    fn from(uuid: Uuid) -> Self {
        Self(uuid)
    }
}

/// Order of a chunk.
///
/// This is used for:
/// 1. **upsert order:** chunks with higher order overwrite data in chunks with lower order
/// 2. **locking order:** chunks must be locked in consistent (ascending) order
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ChunkOrder(NonZeroU32);

impl ChunkOrder {
    // TODO: remove `unsafe` once https://github.com/rust-lang/rust/issues/51999 is fixed
    pub const MIN: Self = Self(unsafe { NonZeroU32::new_unchecked(1) });
    pub const MAX: Self = Self(unsafe { NonZeroU32::new_unchecked(u32::MAX) });

    pub fn new(order: u32) -> Option<Self> {
        NonZeroU32::new(order).map(Self)
    }

    pub fn get(&self) -> u32 {
        self.0.get()
    }

    /// Get next chunk order.
    ///
    /// # Panic
    /// Panics if `self` is already [max](Self::MAX).
    pub fn next(&self) -> Self {
        Self(
            NonZeroU32::new(self.0.get().checked_add(1).expect("chunk order overflow"))
                .expect("did not overflow, so cannot be zero"),
        )
    }
}

impl std::fmt::Display for ChunkOrder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("ChunkOrder").field(&self.0.get()).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_chunk_id_new() {
        // `ChunkId::new()` create new random ID
        assert_ne!(ChunkId::new(), ChunkId::new());
    }

    #[test]
    fn test_chunk_id_new_test() {
        // `ChunkId::new_test(...)` creates deterministic ID
        assert_eq!(ChunkId::new_test(1), ChunkId::new_test(1));
        assert_ne!(ChunkId::new_test(1), ChunkId::new_test(2));
    }

    #[test]
    fn test_chunk_id_debug_and_display() {
        // Random chunk IDs use UUID-format
        let id_random = ChunkId::new();
        let inner: Uuid = id_random.get();
        assert_eq!(
            format!("{:?}", id_random),
            format!("ChunkId({})", inner.to_string())
        );
        assert_eq!(
            format!("{}", id_random),
            format!("ChunkId({})", inner.to_string())
        );

        // Deterministic IDs use integer format
        let id_test = ChunkId::new_test(42);
        assert_eq!(format!("{:?}", id_test), "ChunkId(42)");
        assert_eq!(format!("{}", id_test), "ChunkId(42)");
    }
}
