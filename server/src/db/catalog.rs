//! This module contains the implementation of the InfluxDB IOx Metadata catalog
use std::collections::BTreeSet;
use std::sync::Arc;

use data_types::chunk_metadata::ChunkId;
use data_types::chunk_metadata::ChunkOrder;
use hashbrown::{HashMap, HashSet};

use data_types::chunk_metadata::ChunkSummary;
use data_types::chunk_metadata::DetailedChunkSummary;
use data_types::partition_metadata::{PartitionAddr, PartitionSummary, TableSummary};
use snafu::{OptionExt, Snafu};
use tracker::{
    MappedRwLockReadGuard, MappedRwLockWriteGuard, RwLock, RwLockReadGuard, RwLockWriteGuard,
};

use self::chunk::CatalogChunk;
use self::metrics::CatalogMetrics;
use self::partition::Partition;
use self::table::Table;
use data_types::write_summary::WriteSummary;
use time::TimeProvider;

pub mod chunk;
mod metrics;
pub mod partition;
pub mod table;

#[derive(Debug, Snafu)]
pub enum Error {
    #[snafu(display("table '{}' not found", table))]
    TableNotFound { table: String },

    #[snafu(display("partition '{}' not found in table '{}'", partition, table))]
    PartitionNotFound { partition: String, table: String },

    #[snafu(display(
        "chunk: {} not found in partition '{}' and table '{}'",
        chunk_id,
        partition,
        table
    ))]
    ChunkNotFound {
        chunk_id: ChunkId,
        partition: String,
        table: String,
    },
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Specify which tables are to be matched when filtering
/// catalog chunks
#[derive(Debug, Clone, Copy)]
pub enum TableNameFilter<'a> {
    /// Include all tables
    AllTables,
    /// Only include tables that appear in the named set
    NamedTables(&'a BTreeSet<String>),
}

impl<'a> From<Option<&'a BTreeSet<String>>> for TableNameFilter<'a> {
    /// Creates a [`TableNameFilter`] from an [`Option`].
    ///
    /// If the Option is `None`, all table names will be included in
    /// the results.
    ///
    /// If the Option is `Some(set)`, only table names which apear in
    /// `set` will be included in the results.
    ///
    /// Note `Some(empty set)` will not match anything
    fn from(v: Option<&'a BTreeSet<String>>) -> Self {
        match v {
            Some(names) => Self::NamedTables(names),
            None => Self::AllTables,
        }
    }
}

/// InfluxDB IOx Metadata Catalog
///
/// The Catalog stores information such as which chunks exist, what
/// state they are in, and what objects on object store are used, etc.
///
/// The catalog is also responsible for (eventually) persisting this
/// information
#[derive(Debug)]
pub struct Catalog {
    db_name: Arc<str>,

    /// key is table name
    ///
    /// TODO: Remove this unnecessary additional layer of locking
    tables: RwLock<HashMap<Arc<str>, Table>>,

    metrics: Arc<CatalogMetrics>,

    time_provider: Arc<dyn TimeProvider>,
}

impl Catalog {
    #[cfg(test)]
    fn test() -> Self {
        Self::new(
            Arc::from("test"),
            Default::default(),
            Arc::new(time::SystemProvider::new()),
        )
    }

    pub fn new(
        db_name: Arc<str>,
        metric_registry: Arc<::metric::Registry>,
        time_provider: Arc<dyn TimeProvider>,
    ) -> Self {
        let metrics = Arc::new(CatalogMetrics::new(Arc::clone(&db_name), metric_registry));

        Self {
            db_name,
            tables: Default::default(),
            metrics,
            time_provider,
        }
    }

    /// List all partitions in this database
    pub fn partitions(&self) -> Vec<Arc<RwLock<Partition>>> {
        self.tables
            .read()
            .values()
            .flat_map(|table| table.partitions().cloned())
            .collect()
    }

    /// Get a specific table by name, returning `None` if there is no such table
    pub fn table(&self, table_name: impl AsRef<str>) -> Result<MappedRwLockReadGuard<'_, Table>> {
        let table_name = table_name.as_ref();
        RwLockReadGuard::try_map(self.tables.read(), |tables| tables.get(table_name))
            .map_err(|_| TableNotFound { table: table_name }.build())
    }

    /// Gets or creates a specific table by name
    pub fn get_or_create_table(
        &self,
        table_name: impl AsRef<str>,
    ) -> MappedRwLockWriteGuard<'_, Table> {
        RwLockWriteGuard::map(self.tables.write(), |tables| {
            tables
                .raw_entry_mut()
                .from_key(table_name.as_ref())
                .or_insert_with(|| {
                    let table_name = Arc::from(table_name.as_ref());
                    let table = Table::new(
                        Arc::clone(&self.db_name),
                        Arc::clone(&table_name),
                        self.metrics.new_table_metrics(table_name.as_ref()),
                        Arc::clone(&self.time_provider),
                    );

                    (table_name, table)
                })
                .1
        })
    }

    /// Get a specific partition by name, returning an error if it can't be found
    pub fn partition(
        &self,
        table_name: impl AsRef<str>,
        partition_key: impl AsRef<str>,
    ) -> Result<Arc<RwLock<Partition>>> {
        let table_name = table_name.as_ref();
        let partition_key = partition_key.as_ref();

        self.table(table_name)?
            .partition(partition_key)
            .cloned()
            .context(PartitionNotFound {
                partition: partition_key,
                table: table_name,
            })
    }

    /// Get a specific chunk and its order returning an error if it can't be found
    pub fn chunk(
        &self,
        table_name: impl AsRef<str>,
        partition_key: impl AsRef<str>,
        chunk_id: ChunkId,
    ) -> Result<(Arc<RwLock<CatalogChunk>>, ChunkOrder)> {
        let table_name = table_name.as_ref();
        let partition_key = partition_key.as_ref();

        self.partition(table_name, partition_key)?
            .read()
            .chunk(chunk_id)
            .map(|(chunk, order)| (Arc::clone(chunk), order))
            .context(ChunkNotFound {
                partition: partition_key,
                table: table_name,
                chunk_id,
            })
    }

    /// List all partition keys in this database
    pub fn partition_keys(&self) -> HashSet<String> {
        let mut set = HashSet::new();
        let tables = self.tables.read();
        for table in tables.values() {
            for partition in table.partition_keys() {
                set.get_or_insert_with(partition.as_ref(), ToString::to_string);
            }
        }
        set
    }

    /// Gets or creates a new partition in the catalog
    pub fn get_or_create_partition(
        &self,
        table_name: impl AsRef<str>,
        partition_key: impl AsRef<str>,
    ) -> Arc<RwLock<Partition>> {
        let mut table = self.get_or_create_table(table_name);
        Arc::clone(table.get_or_create_partition(partition_key))
    }

    /// Returns a list of summaries for each partition.
    pub fn partition_summaries(&self) -> Vec<PartitionSummary> {
        self.tables
            .read()
            .values()
            .flat_map(|table| table.partition_summaries())
            .collect()
    }

    /// Returns a list of persistence window summaries for each partition
    pub fn persistence_summaries(&self) -> Vec<(PartitionAddr, WriteSummary)> {
        let mut summaries = Vec::new();
        let tables = self.tables.read();
        for table in tables.values() {
            for partition in table.partitions() {
                let partition = partition.read();
                if let Some(w) = partition.persistence_windows() {
                    for summary in w.summaries() {
                        summaries.push((partition.addr().clone(), summary))
                    }
                }
            }
        }
        summaries
    }

    pub fn chunk_summaries(&self) -> Vec<ChunkSummary> {
        let partition_key = None;
        let table_names = TableNameFilter::AllTables;
        self.filtered_chunks(table_names, partition_key, CatalogChunk::summary)
    }

    pub fn detailed_chunk_summaries(&self) -> Vec<(Arc<TableSummary>, DetailedChunkSummary)> {
        let partition_key = None;
        let table_names = TableNameFilter::AllTables;
        // TODO: Having two summaries with overlapping information seems unfortunate
        self.filtered_chunks(table_names, partition_key, |chunk| {
            (chunk.table_summary(), chunk.detailed_summary())
        })
    }

    /// Returns all chunks within the catalog in an arbitrary order
    pub fn chunks(&self) -> Vec<Arc<RwLock<CatalogChunk>>> {
        let mut chunks = Vec::new();
        let tables = self.tables.read();

        for table in tables.values() {
            for partition in table.partitions() {
                let partition = partition.read();
                chunks.extend(partition.chunks().into_iter().cloned())
            }
        }
        chunks
    }

    /// Calls `map` with every chunk and returns a collection of the results
    ///
    /// If `partition_key` is Some(partition_key) only returns chunks
    /// from the specified partition.
    ///
    /// `table_names` specifies which tables to include
    pub fn filtered_chunks<F, C>(
        &self,
        table_names: TableNameFilter<'_>,
        partition_key: Option<&str>,
        map: F,
    ) -> Vec<C>
    where
        F: Fn(&CatalogChunk) -> C + Copy,
    {
        let tables = self.tables.read();
        let tables = match table_names {
            TableNameFilter::AllTables => itertools::Either::Left(tables.values()),
            TableNameFilter::NamedTables(named_tables) => itertools::Either::Right(
                named_tables
                    .iter()
                    .flat_map(|table_name| tables.get(table_name.as_str()).into_iter()),
            ),
        };

        let partitions = tables.flat_map(|table| match partition_key {
            Some(partition_key) => {
                itertools::Either::Left(table.partition(partition_key).into_iter())
            }
            None => itertools::Either::Right(table.partitions()),
        });

        let mut chunks = Vec::with_capacity(partitions.size_hint().1.unwrap_or_default());
        for partition in partitions {
            let partition = partition.read();
            chunks.extend(partition.chunks().into_iter().map(|chunk| {
                let chunk = chunk.read();
                map(&chunk)
            }))
        }
        chunks
    }

    /// Return a list of all table names in the catalog
    pub fn table_names(&self) -> Vec<String> {
        self.tables.read().keys().map(ToString::to_string).collect()
    }

    pub fn metrics(&self) -> &CatalogMetrics {
        &self.metrics
    }
}

#[cfg(test)]
mod tests {
    use data_types::chunk_metadata::ChunkAddr;
    use mutable_buffer::test_helpers::write_lp_to_new_chunk;

    use super::*;

    fn create_open_chunk(partition: &Arc<RwLock<Partition>>) -> ChunkAddr {
        let mut partition = partition.write();
        let table = partition.table_name();
        let mb_chunk = write_lp_to_new_chunk(&format!("{} bar=1 10", table));
        let chunk = partition.create_open_chunk(mb_chunk);
        let chunk = chunk.read();
        chunk.addr().clone()
    }

    #[test]
    fn partition_get() {
        let catalog = Catalog::test();
        catalog.get_or_create_partition("foo", "p1");
        catalog.get_or_create_partition("foo", "p2");

        let p1 = catalog.partition("foo", "p1").unwrap();
        assert_eq!(p1.read().key(), "p1");

        let p2 = catalog.partition("foo", "p2").unwrap();
        assert_eq!(p2.read().key(), "p2");

        let err = catalog.partition("foo", "p3").unwrap_err();
        assert_eq!(err.to_string(), "partition 'p3' not found in table 'foo'");
    }

    #[test]
    fn partition_list() {
        let catalog = Catalog::test();

        assert_eq!(catalog.partitions().len(), 0);

        catalog.get_or_create_partition("t1", "p1");
        catalog.get_or_create_partition("t2", "p2");
        catalog.get_or_create_partition("t1", "p3");

        let mut partition_keys: Vec<String> = catalog
            .partitions()
            .into_iter()
            .map(|p| p.read().key().into())
            .collect();
        partition_keys.sort_unstable();

        assert_eq!(partition_keys, vec!["p1", "p2", "p3"]);
    }

    #[test]
    fn chunk_create() {
        let catalog = Catalog::test();
        let p1 = catalog.get_or_create_partition("t1", "p1");
        let p2 = catalog.get_or_create_partition("t2", "p2");

        let addr1 = create_open_chunk(&p1);
        let addr2 = create_open_chunk(&p1);
        let addr3 = create_open_chunk(&p2);

        let p1 = p1.write();
        let p2 = p2.write();

        let (c1_0, _order) = p1.chunk(addr1.chunk_id).unwrap();
        assert_eq!(c1_0.read().table_name().as_ref(), "t1");
        assert_eq!(c1_0.read().key(), "p1");
        assert_eq!(c1_0.read().id(), addr1.chunk_id);

        let (c1_1, _order) = p1.chunk(addr2.chunk_id).unwrap();
        assert_eq!(c1_1.read().table_name().as_ref(), "t1");
        assert_eq!(c1_1.read().key(), "p1");
        assert_eq!(c1_1.read().id(), addr2.chunk_id);

        let (c2_0, _order) = p2.chunk(addr3.chunk_id).unwrap();
        assert_eq!(c2_0.read().table_name().as_ref(), "t2");
        assert_eq!(c2_0.read().key(), "p2");
        assert_eq!(c2_0.read().id(), addr3.chunk_id);

        assert!(p1.chunk(ChunkId::new_test(100)).is_none());
    }

    #[test]
    fn chunk_list() {
        let catalog = Catalog::test();

        let p1 = catalog.get_or_create_partition("table1", "p1");
        let p2 = catalog.get_or_create_partition("table2", "p1");
        let addr1 = create_open_chunk(&p1);
        let addr2 = create_open_chunk(&p1);
        let addr3 = create_open_chunk(&p2);

        let p3 = catalog.get_or_create_partition("table1", "p2");
        let addr4 = create_open_chunk(&p3);

        assert_eq!(
            chunk_addrs(&catalog),
            as_sorted(vec![addr1, addr2, addr3, addr4,]),
        );
    }

    fn chunk_addrs(catalog: &Catalog) -> Vec<ChunkAddr> {
        let mut chunks: Vec<_> = catalog
            .partitions()
            .into_iter()
            .flat_map(|p| {
                let p = p.read();
                p.chunks()
                    .into_iter()
                    .map(|c| {
                        let c = c.read();
                        c.addr().clone()
                    })
                    .collect::<Vec<_>>()
                    .into_iter()
            })
            .collect();

        chunks.sort();
        chunks
    }

    #[test]
    fn chunk_drop() {
        let catalog = Catalog::test();

        let p1 = catalog.get_or_create_partition("p1", "table1");
        let p2 = catalog.get_or_create_partition("p1", "table2");
        let addr1 = create_open_chunk(&p1);
        let addr2 = create_open_chunk(&p1);
        let addr3 = create_open_chunk(&p2);

        let p3 = catalog.get_or_create_partition("p2", "table1");
        let _addr4 = create_open_chunk(&p3);

        assert_eq!(chunk_addrs(&catalog).len(), 4);

        {
            let mut p2 = p2.write();
            p2.drop_chunk(addr3.chunk_id).unwrap();
            assert!(p2.chunk(addr3.chunk_id).is_none()); // chunk is gone
        }
        assert_eq!(chunk_addrs(&catalog).len(), 3);

        {
            let mut p1 = p1.write();
            p1.drop_chunk(addr2.chunk_id).unwrap();
            assert!(p1.chunk(addr2.chunk_id).is_none()); // chunk is gone
        }
        assert_eq!(chunk_addrs(&catalog).len(), 2);

        {
            let mut p1 = p1.write();
            p1.drop_chunk(addr1.chunk_id).unwrap();
            assert!(p1.chunk(addr1.chunk_id).is_none()); // chunk is gone
        }
        assert_eq!(chunk_addrs(&catalog).len(), 1);
    }

    #[test]
    fn chunk_drop_non_existent_chunk() {
        let catalog = Catalog::test();
        let p3 = catalog.get_or_create_partition("table1", "p3");
        create_open_chunk(&p3);

        let mut p3 = p3.write();
        let err = p3.drop_chunk(ChunkId::new_test(1337)).unwrap_err();

        assert!(matches!(err, partition::Error::ChunkNotFound { .. }))
    }

    #[test]
    fn chunk_recreate_dropped() {
        let catalog = Catalog::test();

        let p1 = catalog.get_or_create_partition("table1", "p1");
        let addr1 = create_open_chunk(&p1);
        let addr2 = create_open_chunk(&p1);
        assert_eq!(
            chunk_addrs(&catalog),
            as_sorted(vec![addr1.clone(), addr2.clone(),]),
        );

        {
            let mut p1 = p1.write();
            p1.drop_chunk(addr1.chunk_id).unwrap();
        }
        assert_eq!(chunk_addrs(&catalog), vec![addr2.clone()]);

        // should be ok to "re-create", it gets another chunk_id though
        let addr3 = create_open_chunk(&p1);
        assert_eq!(chunk_addrs(&catalog), as_sorted(vec![addr2, addr3,]),);
    }

    #[test]
    fn filtered_chunks() {
        use TableNameFilter::*;
        let catalog = Catalog::test();

        let p1 = catalog.get_or_create_partition("table1", "p1");
        let p2 = catalog.get_or_create_partition("table2", "p1");
        let p3 = catalog.get_or_create_partition("table2", "p2");
        create_open_chunk(&p1);
        create_open_chunk(&p2);
        create_open_chunk(&p3);

        let a = catalog.filtered_chunks(AllTables, None, |_| ());

        let b = catalog.filtered_chunks(NamedTables(&make_set("table1")), None, |_| ());

        let c = catalog.filtered_chunks(NamedTables(&make_set("table2")), None, |_| ());

        let d = catalog.filtered_chunks(NamedTables(&make_set("table2")), Some("p2"), |_| ());

        assert_eq!(a.len(), 3);
        assert_eq!(b.len(), 1);
        assert_eq!(c.len(), 2);
        assert_eq!(d.len(), 1);
    }

    fn make_set(s: impl Into<String>) -> BTreeSet<String> {
        std::iter::once(s.into()).collect()
    }

    fn as_sorted<T>(mut v: Vec<T>) -> Vec<T>
    where
        T: Ord,
    {
        v.sort();
        v
    }
}
