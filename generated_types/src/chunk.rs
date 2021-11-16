use crate::google::OptionalField;
use crate::{
    google::{FieldViolation, FieldViolationExt, FromOptionalField},
    influxdata::iox::management::v1 as management,
};
use data_types::chunk_metadata::{
    ChunkId, ChunkLifecycleAction, ChunkOrder, ChunkStorage, ChunkSummary,
};
use std::{
    convert::{TryFrom, TryInto},
    sync::Arc,
};
use time::Time;

/// Conversion code to management API chunk structure
impl From<ChunkSummary> for management::Chunk {
    fn from(summary: ChunkSummary) -> Self {
        let ChunkSummary {
            partition_key,
            table_name,
            id,
            storage,
            lifecycle_action,
            memory_bytes,
            object_store_bytes,
            row_count,
            time_of_last_access,
            time_of_first_write,
            time_of_last_write,
            order,
        } = summary;

        Self {
            partition_key: partition_key.to_string(),
            table_name: table_name.to_string(),
            id: id.into(),
            storage: management::ChunkStorage::from(storage).into(),
            lifecycle_action: management::ChunkLifecycleAction::from(lifecycle_action).into(),
            memory_bytes: memory_bytes as u64,
            object_store_bytes: object_store_bytes as u64,
            row_count: row_count as u64,
            time_of_last_access: time_of_last_access.map(|t| t.date_time().into()),
            time_of_first_write: Some(time_of_first_write.date_time().into()),
            time_of_last_write: Some(time_of_last_write.date_time().into()),
            order: order.get(),
        }
    }
}

impl From<ChunkStorage> for management::ChunkStorage {
    fn from(storage: ChunkStorage) -> Self {
        match storage {
            ChunkStorage::OpenMutableBuffer => Self::OpenMutableBuffer,
            ChunkStorage::ClosedMutableBuffer => Self::ClosedMutableBuffer,
            ChunkStorage::ReadBuffer => Self::ReadBuffer,
            ChunkStorage::ReadBufferAndObjectStore => Self::ReadBufferAndObjectStore,
            ChunkStorage::ObjectStoreOnly => Self::ObjectStoreOnly,
        }
    }
}

impl From<Option<ChunkLifecycleAction>> for management::ChunkLifecycleAction {
    fn from(lifecycle_action: Option<ChunkLifecycleAction>) -> Self {
        match lifecycle_action {
            Some(ChunkLifecycleAction::Persisting) => Self::Persisting,
            Some(ChunkLifecycleAction::Compacting) => Self::Compacting,
            Some(ChunkLifecycleAction::CompactingObjectStore) => Self::CompactingObjectStore,
            Some(ChunkLifecycleAction::Dropping) => Self::Dropping,
            None => Self::Unspecified,
        }
    }
}

/// Conversion code from management API chunk structure
impl TryFrom<management::Chunk> for ChunkSummary {
    type Error = FieldViolation;

    fn try_from(proto: management::Chunk) -> Result<Self, Self::Error> {
        let convert_timestamp = |t: pbjson_types::Timestamp, field: &'static str| {
            let date_time = t.try_into().map_err(|_| FieldViolation {
                field: field.to_string(),
                description: "Timestamp must be positive".to_string(),
            })?;
            Ok(Time::from_date_time(date_time))
        };

        let timestamp = |t: Option<pbjson_types::Timestamp>, field: &'static str| {
            t.map(|t| convert_timestamp(t, field)).transpose()
        };

        let required_timestamp = |t: Option<pbjson_types::Timestamp>, field: &'static str| {
            t.unwrap_field(field)
                .and_then(|t| convert_timestamp(t, field))
        };

        let management::Chunk {
            partition_key,
            table_name,
            id,
            storage,
            lifecycle_action,
            memory_bytes,
            object_store_bytes,
            row_count,
            time_of_last_access,
            time_of_first_write,
            time_of_last_write,
            order,
        } = proto;

        Ok(Self {
            partition_key: Arc::from(partition_key.as_str()),
            table_name: Arc::from(table_name.as_str()),
            id: ChunkId::try_from(id).scope("id")?,
            storage: management::ChunkStorage::from_i32(storage).required("storage")?,
            lifecycle_action: management::ChunkLifecycleAction::from_i32(lifecycle_action)
                .required("lifecycle_action")?,
            memory_bytes: memory_bytes as usize,
            object_store_bytes: object_store_bytes as usize,
            row_count: row_count as usize,
            time_of_last_access: timestamp(time_of_last_access, "time_of_last_access")?,
            time_of_first_write: required_timestamp(time_of_first_write, "time_of_first_write")?,
            time_of_last_write: required_timestamp(time_of_last_write, "time_of_last_write")?,
            order: ChunkOrder::new(order).unwrap_field("order")?,
        })
    }
}

impl TryFrom<management::ChunkStorage> for ChunkStorage {
    type Error = FieldViolation;

    fn try_from(proto: management::ChunkStorage) -> Result<Self, Self::Error> {
        match proto {
            management::ChunkStorage::OpenMutableBuffer => Ok(Self::OpenMutableBuffer),
            management::ChunkStorage::ClosedMutableBuffer => Ok(Self::ClosedMutableBuffer),
            management::ChunkStorage::ReadBuffer => Ok(Self::ReadBuffer),
            management::ChunkStorage::ReadBufferAndObjectStore => {
                Ok(Self::ReadBufferAndObjectStore)
            }
            management::ChunkStorage::ObjectStoreOnly => Ok(Self::ObjectStoreOnly),
            management::ChunkStorage::Unspecified => Err(FieldViolation::required("")),
        }
    }
}

impl TryFrom<management::ChunkLifecycleAction> for Option<ChunkLifecycleAction> {
    type Error = FieldViolation;

    fn try_from(proto: management::ChunkLifecycleAction) -> Result<Self, Self::Error> {
        match proto {
            management::ChunkLifecycleAction::Persisting => {
                Ok(Some(ChunkLifecycleAction::Persisting))
            }
            management::ChunkLifecycleAction::Compacting => {
                Ok(Some(ChunkLifecycleAction::Compacting))
            }
            management::ChunkLifecycleAction::CompactingObjectStore => {
                Ok(Some(ChunkLifecycleAction::CompactingObjectStore))
            }
            management::ChunkLifecycleAction::Dropping => Ok(Some(ChunkLifecycleAction::Dropping)),
            management::ChunkLifecycleAction::Unspecified => Ok(None),
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use bytes::Bytes;
    use data_types::chunk_metadata::ChunkOrder;
    use time::Time;

    #[test]
    fn valid_proto_to_summary() {
        let now = Time::from_timestamp(2, 6);
        let proto = management::Chunk {
            partition_key: "foo".to_string(),
            table_name: "bar".to_string(),
            id: Bytes::from("\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0*"),
            memory_bytes: 1234,
            object_store_bytes: 567,
            row_count: 321,

            storage: management::ChunkStorage::ObjectStoreOnly.into(),
            lifecycle_action: management::ChunkLifecycleAction::Compacting.into(),
            time_of_first_write: Some(now.date_time().into()),
            time_of_last_write: Some(now.date_time().into()),
            time_of_last_access: Some(pbjson_types::Timestamp {
                seconds: 50,
                nanos: 7,
            }),
            order: 5,
        };

        let summary = ChunkSummary::try_from(proto).expect("conversion successful");
        let expected = ChunkSummary {
            partition_key: Arc::from("foo"),
            table_name: Arc::from("bar"),
            id: ChunkId::new_test(42),
            memory_bytes: 1234,
            object_store_bytes: 567,
            row_count: 321,
            storage: ChunkStorage::ObjectStoreOnly,
            lifecycle_action: Some(ChunkLifecycleAction::Compacting),
            time_of_first_write: now,
            time_of_last_write: now,
            time_of_last_access: Some(Time::from_timestamp_nanos(50_000_000_007)),
            order: ChunkOrder::new(5).unwrap(),
        };

        assert_eq!(
            summary, expected,
            "Actual:\n\n{:?}\n\nExpected:\n\n{:?}\n\n",
            summary, expected
        );
    }

    #[test]
    fn valid_summary_to_proto() {
        let now = Time::from_timestamp(756, 23);
        let summary = ChunkSummary {
            partition_key: Arc::from("foo"),
            table_name: Arc::from("bar"),
            id: ChunkId::new_test(42),
            memory_bytes: 1234,
            object_store_bytes: 567,
            row_count: 321,
            storage: ChunkStorage::ObjectStoreOnly,
            lifecycle_action: Some(ChunkLifecycleAction::Persisting),
            time_of_first_write: now,
            time_of_last_write: now,
            time_of_last_access: Some(Time::from_timestamp_nanos(12_000_100_007)),
            order: ChunkOrder::new(5).unwrap(),
        };

        let proto = management::Chunk::try_from(summary).expect("conversion successful");

        let expected = management::Chunk {
            partition_key: "foo".to_string(),
            table_name: "bar".to_string(),
            id: Bytes::from("\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0*"),
            memory_bytes: 1234,
            object_store_bytes: 567,
            row_count: 321,
            storage: management::ChunkStorage::ObjectStoreOnly.into(),
            lifecycle_action: management::ChunkLifecycleAction::Persisting.into(),
            time_of_first_write: Some(now.date_time().into()),
            time_of_last_write: Some(now.date_time().into()),
            time_of_last_access: Some(pbjson_types::Timestamp {
                seconds: 12,
                nanos: 100_007,
            }),
            order: 5,
        };

        assert_eq!(
            proto, expected,
            "Actual:\n\n{:?}\n\nExpected:\n\n{:?}\n\n",
            proto, expected
        );
    }
}
