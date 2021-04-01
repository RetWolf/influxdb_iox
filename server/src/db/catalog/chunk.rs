use std::collections::BTreeSet;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use data_types::{chunk::ChunkSummary, partition_metadata::TableSummary};
use mutable_buffer::chunk::Chunk as MBChunk;
use parquet_file::chunk::Chunk as ParquetChunk;
use query::PartitionChunk;
use read_buffer::Database as ReadBufferDb;

use crate::db::DBChunk;

use super::{InternalChunkState, Result};

/// The state a Chunk is in and what its underlying backing storage is
#[derive(Debug)]
pub enum ChunkState {
    /// An invalid chunk state that should not be externally observed
    ///
    /// Used internally to allow moving data between enum variants
    Invalid,

    /// Chunk can accept new writes
    Open(MBChunk),

    /// Chunk can still accept new writes, but will likely be closed soon
    Closing(MBChunk),

    /// Chunk is closed for new writes, and is actively moving to the read
    /// buffer
    Moving(Arc<MBChunk>),

    /// Chunk has been completely loaded in the read buffer
    Moved(Arc<ReadBufferDb>), // todo use read buffer chunk instead of ReadBufferDb

    // Chunk is actively writing to object store
    WritingToObjectStore(Arc<ReadBufferDb>), // todo use read buffer chunk instead of ReadBufferD

    // Chunk has been completely written into object store
    WrittenToObjectStore(Arc<ReadBufferDb>, Arc<ParquetChunk>),
}

impl ChunkState {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Invalid => "Invalid",
            Self::Open(_) => "Open",
            Self::Closing(_) => "Closing",
            Self::Moving(_) => "Moving",
            Self::Moved(_) => "Moved",
            Self::WritingToObjectStore(_) => "Writing to Object Store",
            Self::WrittenToObjectStore(_, _) => "Written to Object Store",
        }
    }
}

/// The catalog representation of a Chunk in IOx. Note that a chunk
/// may exist in several physical locations at any given time (e.g. in
/// mutable buffer and in read buffer)
#[derive(Debug)]
pub struct Chunk {
    /// What partition does the chunk belong to?
    partition_key: Arc<String>,

    /// The ID of the chunk
    id: u32,

    /// The state of this chunk
    state: ChunkState,

    /// Time at which the first data was written into this chunk. Note
    /// this is not the same as the timestamps on the data itself
    time_of_first_write: Option<DateTime<Utc>>,

    /// Most recent time at which data write was initiated into this
    /// chunk. Note this is not the same as the timestamps on the data
    /// itself
    time_of_last_write: Option<DateTime<Utc>>,

    /// Time at which this chunk was maked as closing. Note this is
    /// not the same as the timestamps on the data itself
    time_closing: Option<DateTime<Utc>>,
}

macro_rules! unexpected_state {
    ($SELF: expr, $OP: expr, $EXPECTED: expr, $STATE: expr) => {
        InternalChunkState {
            partition_key: $SELF.partition_key.as_str(),
            chunk_id: $SELF.id,
            operation: $OP,
            expected: $EXPECTED,
            actual: $STATE.name(),
        }
        .fail()
    };
}

impl Chunk {
    /// Create a new chunk in the provided state
    pub(crate) fn new(partition_key: impl Into<String>, id: u32, state: ChunkState) -> Self {
        Self {
            partition_key: Arc::new(partition_key.into()),
            id,
            state,
            time_of_first_write: None,
            time_of_last_write: None,
            time_closing: None,
        }
    }

    /// Creates a new open chunk
    pub(crate) fn new_open(partition_key: impl Into<String>, id: u32) -> Self {
        let state = ChunkState::Open(mutable_buffer::chunk::Chunk::new(id));
        Self::new(partition_key, id, state)
    }

    /// Used for testing
    #[cfg(test)]
    pub(crate) fn set_timestamps(
        &mut self,
        time_of_first_write: Option<DateTime<Utc>>,
        time_of_last_write: Option<DateTime<Utc>>,
    ) {
        self.time_of_first_write = time_of_first_write;
        self.time_of_last_write = time_of_last_write;
    }

    pub fn id(&self) -> u32 {
        self.id
    }

    pub fn key(&self) -> &str {
        self.partition_key.as_ref()
    }

    pub fn state(&self) -> &ChunkState {
        &self.state
    }

    pub fn time_of_first_write(&self) -> Option<DateTime<Utc>> {
        self.time_of_first_write
    }

    pub fn time_of_last_write(&self) -> Option<DateTime<Utc>> {
        self.time_of_last_write
    }

    pub fn time_closing(&self) -> Option<DateTime<Utc>> {
        self.time_closing
    }

    /// Update the write timestamps for this chunk
    pub fn record_write(&mut self) {
        let now = Utc::now();
        if self.time_of_first_write.is_none() {
            self.time_of_first_write = Some(now);
        }
        self.time_of_last_write = Some(now);
    }

    /// Return ChunkSummary metadata for this chunk
    pub fn summary(&self) -> ChunkSummary {
        ChunkSummary {
            time_of_first_write: self.time_of_first_write,
            time_of_last_write: self.time_of_last_write,
            time_closing: self.time_closing,
            ..DBChunk::snapshot(self).summary()
        }
    }

    /// Return TableSummary metadata for each table in this chunk
    pub fn table_summaries(&self) -> impl Iterator<Item = TableSummary> {
        DBChunk::snapshot(self).table_summaries().into_iter()
    }

    /// Returns true if this chunk contains a table with the provided name
    pub fn has_table(&self, table_name: &str) -> bool {
        match &self.state {
            ChunkState::Invalid => false,
            ChunkState::Open(chunk) | ChunkState::Closing(chunk) => chunk.has_table(table_name),
            ChunkState::Moving(chunk) => chunk.has_table(table_name),
            ChunkState::Moved(db) => {
                db.has_table(self.partition_key.as_str(), table_name, &[self.id])
            }
            ChunkState::WritingToObjectStore(db) => {
                db.has_table(self.partition_key.as_str(), table_name, &[self.id])
            }
            ChunkState::WrittenToObjectStore(db, _) => {
                db.has_table(self.partition_key.as_str(), table_name, &[self.id])
            }
        }
    }

    /// Collects the chunk's table names into `names`
    pub fn table_names(&self, names: &mut BTreeSet<String>) {
        match &self.state {
            ChunkState::Invalid => {}
            ChunkState::Open(chunk) | ChunkState::Closing(chunk) => chunk.all_table_names(names),
            ChunkState::Moving(chunk) => chunk.all_table_names(names),
            ChunkState::Moved(db) => {
                db.all_table_names(self.partition_key.as_str(), &[self.id], names)
            }
            ChunkState::WritingToObjectStore(db) => {
                db.all_table_names(self.partition_key.as_str(), &[self.id], names)
            }
            ChunkState::WrittenToObjectStore(db, _) => {
                db.all_table_names(self.partition_key.as_str(), &[self.id], names)
            }
        }
    }

    /// Returns an approximation of the amount of process memory consumed by the
    /// chunk
    pub fn size(&self) -> usize {
        match &self.state {
            ChunkState::Invalid => 0,
            ChunkState::Open(chunk) | ChunkState::Closing(chunk) => chunk.size(),
            ChunkState::Moving(chunk) => chunk.size(),
            ChunkState::Moved(db) => db
                .chunks_size(self.partition_key.as_str(), &[self.id])
                .unwrap_or(0) as usize,
            ChunkState::WritingToObjectStore(db) => db
                .chunks_size(self.partition_key.as_str(), &[self.id])
                .unwrap_or(0) as usize,
            ChunkState::WrittenToObjectStore(db, parquet_chunk) => {
                parquet_chunk.size()
                    + db.chunks_size(self.partition_key.as_str(), &[self.id])
                        .unwrap_or(0) as usize
            }
        }
    }

    /// Returns a mutable reference to the mutable buffer storage for
    /// chunks in the Open or Closing state
    ///
    /// Must be in open or closing state
    pub fn mutable_buffer(&mut self) -> Result<&mut MBChunk> {
        match &mut self.state {
            ChunkState::Open(chunk) => Ok(chunk),
            ChunkState::Closing(chunk) => Ok(chunk),
            state => unexpected_state!(self, "mutable buffer reference", "Open or Closing", state),
        }
    }

    /// Set the chunk to the Closing state
    pub fn set_closing(&mut self) -> Result<()> {
        let mut s = ChunkState::Invalid;
        std::mem::swap(&mut s, &mut self.state);

        match s {
            ChunkState::Open(s) | ChunkState::Closing(s) => {
                assert!(self.time_closing.is_none());
                self.time_closing = Some(Utc::now());
                self.state = ChunkState::Closing(s);
                Ok(())
            }
            state => {
                self.state = state;
                unexpected_state!(self, "setting closing", "Open or Closing", &self.state)
            }
        }
    }

    /// Set the chunk to the Moving state, returning a handle to the underlying
    /// storage
    pub fn set_moving(&mut self) -> Result<Arc<MBChunk>> {
        let mut s = ChunkState::Invalid;
        std::mem::swap(&mut s, &mut self.state);

        match s {
            ChunkState::Open(chunk) | ChunkState::Closing(chunk) => {
                let chunk = Arc::new(chunk);
                self.state = ChunkState::Moving(Arc::clone(&chunk));
                Ok(chunk)
            }
            state => {
                self.state = state;
                unexpected_state!(self, "setting moving", "Open or Closing", &self.state)
            }
        }
    }

    /// Set the chunk in the Moved state, setting the underlying
    /// storage handle to db, and discarding the underlying mutable buffer
    /// storage.
    pub fn set_moved(&mut self, db: Arc<ReadBufferDb>) -> Result<()> {
        let mut s = ChunkState::Invalid;
        std::mem::swap(&mut s, &mut self.state);

        match s {
            ChunkState::Moving(_) => {
                self.state = ChunkState::Moved(db);
                Ok(())
            }
            state => {
                self.state = state;
                unexpected_state!(self, "setting moved", "Moving", &self.state)
            }
        }
    }

    /// Set the chunk to the MovingToObjectStore state
    pub fn set_writing_to_object_store(&mut self) -> Result<Arc<ReadBufferDb>> {
        let mut s = ChunkState::Invalid;
        std::mem::swap(&mut s, &mut self.state);

        match s {
            ChunkState::Moved(db) => {
                self.state = ChunkState::WritingToObjectStore(Arc::clone(&db));
                Ok(db)
            }
            state => {
                self.state = state;
                unexpected_state!(self, "setting object store", "Moved", &self.state)
            }
        }
    }

    /// Set the chunk to the MovedToObjectStore state, returning a handle to the
    /// underlying storage
    pub fn set_written_to_object_store(&mut self, chunk: Arc<ParquetChunk>) -> Result<()> {
        let mut s = ChunkState::Invalid;
        std::mem::swap(&mut s, &mut self.state);

        match s {
            ChunkState::WritingToObjectStore(db) => {
                self.state = ChunkState::WrittenToObjectStore(db, chunk);
                Ok(())
            }
            state => {
                self.state = state;
                unexpected_state!(
                    self,
                    "setting object store",
                    "MovingToObjectStore",
                    &self.state
                )
            }
        }
    }
}