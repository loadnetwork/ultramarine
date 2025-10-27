#![allow(clippy::result_large_err)]

use std::{mem::size_of, ops::RangeBounds, path::Path, sync::Arc, time::Instant};

use bytes::Bytes;
use malachitebft_app_channel::app::types::{
    ProposedValue,
    codec::Codec,
    core::{CommitCertificate, Round},
};
use malachitebft_proto::{Error as ProtoError, Protobuf};
use prost::Message;
use redb::{ReadableDatabase, ReadableTable};
use thiserror::Error;
use tracing::error;
use ultramarine_types::{
    blob_metadata::BlobMetadata,
    codec::{proto as codec, proto::ProtobufCodec},
    consensus_block_metadata::ConsensusBlockMetadata,
    context::LoadContext,
    ethereum_compat::SignedBeaconBlockHeader,
    height::Height,
    proto,
    value::Value,
};

mod keys;
use keys::{HeightKey, UndecidedValueKey};

use crate::metrics::DbMetrics;

#[derive(Clone, Debug)]
pub struct DecidedValue {
    pub value: Value,
    pub certificate: CommitCertificate<LoadContext>,
}

fn decode_certificate(bytes: &[u8]) -> Result<CommitCertificate<LoadContext>, ProtoError> {
    let proto = proto::CommitCertificate::decode(bytes)?;
    codec::decode_certificate(proto)
}

fn encode_certificate(certificate: &CommitCertificate<LoadContext>) -> Result<Vec<u8>, ProtoError> {
    let proto = codec::encode_certificate(certificate)?;
    Ok(proto.encode_to_vec())
}

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("Database error: {0}")]
    Database(#[from] redb::DatabaseError),

    #[error("Storage error: {0}")]
    Storage(#[from] redb::StorageError),

    #[error("Table error: {0}")]
    Table(#[from] redb::TableError),

    #[error("Commit error: {0}")]
    Commit(#[from] redb::CommitError),

    #[error("Transaction error: {0}")]
    Transaction(#[from] redb::TransactionError),

    #[error("Failed to encode/decode Protobuf: {0}")]
    Protobuf(#[from] ProtoError),

    #[error("Failed to join on task: {0}")]
    TaskJoin(#[from] tokio::task::JoinError),

    #[error("Blob metadata not found for height {height} round {round}")]
    MissingBlobMetadata { height: u64, round: i64 },
}

const CERTIFICATES_TABLE: redb::TableDefinition<HeightKey, Vec<u8>> =
    redb::TableDefinition::new("certificates");

const DECIDED_VALUES_TABLE: redb::TableDefinition<HeightKey, Vec<u8>> =
    redb::TableDefinition::new("decided_values");

const UNDECIDED_PROPOSALS_TABLE: redb::TableDefinition<UndecidedValueKey, Vec<u8>> =
    redb::TableDefinition::new("undecided_values");

const DECIDED_BLOCK_DATA_TABLE: redb::TableDefinition<HeightKey, Vec<u8>> =
    redb::TableDefinition::new("decided_block_data");

const UNDECIDED_BLOCK_DATA_TABLE: redb::TableDefinition<UndecidedValueKey, Vec<u8>> =
    redb::TableDefinition::new("undecided_block_data");

const BLOCK_HEADERS_TABLE: redb::TableDefinition<HeightKey, Vec<u8>> =
    redb::TableDefinition::new("block_headers");

// Phase 4: Three-layer architecture - metadata storage
// Layer 1: Pure BFT consensus metadata (keep forever)
const CONSENSUS_BLOCK_METADATA_TABLE: redb::TableDefinition<HeightKey, Vec<u8>> =
    redb::TableDefinition::new("consensus_block_metadata");

// Layer 2: Blob metadata (keep forever)
const BLOB_METADATA_DECIDED_TABLE: redb::TableDefinition<HeightKey, Vec<u8>> =
    redb::TableDefinition::new("blob_metadata_decided");

const BLOB_METADATA_UNDECIDED_TABLE: redb::TableDefinition<UndecidedValueKey, Vec<u8>> =
    redb::TableDefinition::new("blob_metadata_undecided");

// Metadata pointer for O(1) latest blob metadata lookup
const BLOB_METADATA_META_TABLE: redb::TableDefinition<&str, Vec<u8>> =
    redb::TableDefinition::new("blob_metadata_meta");

struct Db {
    db: redb::Database,
    metrics: DbMetrics,
}

impl Db {
    fn new(path: impl AsRef<Path>, metrics: DbMetrics) -> Result<Self, StoreError> {
        let db = redb::Database::create(path).map_err(StoreError::Database)?;
        Ok(Self { db, metrics })
    }

    fn get_decided_value(&self, height: Height) -> Result<Option<DecidedValue>, StoreError> {
        let start = Instant::now();
        let mut read_bytes = 0;

        let tx = self.db.begin_read()?;

        let value = {
            let table = tx.open_table(DECIDED_VALUES_TABLE)?;
            let value = table.get(&height)?;
            value.and_then(|value| {
                let bytes = value.value();
                read_bytes = bytes.len() as u64;
                // Deserialize using Protobuf trait
                ProtobufCodec.decode(Bytes::copy_from_slice(&bytes)).ok()
            })
        };

        let certificate = {
            let table = tx.open_table(CERTIFICATES_TABLE)?;
            let value = table.get(&height)?;
            value.and_then(|value| {
                let bytes = value.value();
                read_bytes += bytes.len() as u64;
                decode_certificate(&bytes).ok()
            })
        };

        self.metrics.observe_read_time(start.elapsed());
        self.metrics.add_read_bytes(read_bytes);
        self.metrics.add_key_read_bytes(size_of::<Height>() as u64);

        let decided_value =
            value.zip(certificate).map(|(value, certificate)| DecidedValue { value, certificate });

        Ok(decided_value)
    }

    fn insert_decided_value(&self, decided_value: DecidedValue) -> Result<(), StoreError> {
        let start = Instant::now();
        let mut write_bytes = 0;

        let height = decided_value.certificate.height;
        let tx = self.db.begin_write()?;

        {
            let mut values = tx.open_table(DECIDED_VALUES_TABLE)?;
            // Serialize using Protobuf trait via ProtobufCodec
            let values_bytes = ProtobufCodec.encode(&decided_value.value)?;
            write_bytes += values_bytes.len() as u64;
            values.insert(height, values_bytes.to_vec())?;
        }

        {
            let mut certificates = tx.open_table(CERTIFICATES_TABLE)?;
            let encoded_certificate = encode_certificate(&decided_value.certificate)?;
            write_bytes += encoded_certificate.len() as u64;
            certificates.insert(height, encoded_certificate)?;
        }

        tx.commit()?;

        self.metrics.observe_write_time(start.elapsed());
        self.metrics.add_write_bytes(write_bytes);

        Ok(())
    }

    #[tracing::instrument(skip(self))]
    pub fn get_undecided_proposal(
        &self,
        height: Height,
        round: Round,
    ) -> Result<Option<ProposedValue<LoadContext>>, StoreError> {
        let start = Instant::now();
        let mut read_bytes = 0;

        let tx = self.db.begin_read()?;
        let table = tx.open_table(UNDECIDED_PROPOSALS_TABLE)?;

        let value = if let Ok(Some(value)) = table.get(&(height, round)) {
            let bytes = value.value();
            read_bytes += bytes.len() as u64;

            let proposal =
                ProtobufCodec.decode(Bytes::from(bytes)).map_err(StoreError::Protobuf)?;

            Some(proposal)
        } else {
            None
        };

        self.metrics.observe_read_time(start.elapsed());
        self.metrics.add_read_bytes(read_bytes);
        self.metrics.add_key_read_bytes(size_of::<(Height, Round)>() as u64);

        Ok(value)
    }

    fn insert_undecided_proposal(
        &self,
        proposal: ProposedValue<LoadContext>,
    ) -> Result<(), StoreError> {
        let start = Instant::now();

        let key = (proposal.height, proposal.round);
        let value = ProtobufCodec.encode(&proposal)?;

        let tx = self.db.begin_write()?;
        {
            let mut table = tx.open_table(UNDECIDED_PROPOSALS_TABLE)?;
            // Only insert if no value exists at this key
            if table.get(&key)?.is_none() {
                table.insert(key, value.to_vec())?;
            }
        }
        tx.commit()?;

        self.metrics.observe_write_time(start.elapsed());
        self.metrics.add_write_bytes(value.len() as u64);

        Ok(())
    }

    fn height_range<Table>(
        &self,
        table: &Table,
        range: impl RangeBounds<Height>,
    ) -> Result<Vec<Height>, StoreError>
    where
        Table: redb::ReadableTable<HeightKey, Vec<u8>>,
    {
        Ok(table.range(range)?.flatten().map(|(key, _)| key.value()).collect::<Vec<_>>())
    }

    fn undecided_proposals_range<Table>(
        &self,
        table: &Table,
        range: impl RangeBounds<(Height, Round)>,
    ) -> Result<Vec<(Height, Round)>, StoreError>
    where
        Table: redb::ReadableTable<UndecidedValueKey, Vec<u8>>,
    {
        Ok(table.range(range)?.flatten().map(|(key, _)| key.value()).collect::<Vec<_>>())
    }

    fn block_data_range<Table>(
        &self,
        table: &Table,
        range: impl RangeBounds<(Height, Round)>,
    ) -> Result<Vec<(Height, Round)>, StoreError>
    where
        Table: redb::ReadableTable<UndecidedValueKey, Vec<u8>>,
    {
        Ok(table.range(range)?.flatten().map(|(key, _)| key.value()).collect::<Vec<_>>())
    }

    fn prune(&self, retain_height: Height) -> Result<Vec<Height>, StoreError> {
        let start = Instant::now();

        let tx = self.db.begin_write().unwrap();

        let pruned = {
            let mut undecided = tx.open_table(UNDECIDED_PROPOSALS_TABLE)?;
            let keys = self.undecided_proposals_range(&undecided, ..(retain_height, Round::Nil))?;
            for key in keys {
                undecided.remove(key)?;
            }

            let mut undecided_block_data = tx.open_table(UNDECIDED_BLOCK_DATA_TABLE)?;
            let keys =
                self.block_data_range(&undecided_block_data, ..(retain_height, Round::Nil))?;
            for key in &keys {
                undecided_block_data.remove(key)?;
            }

            let mut decided = tx.open_table(DECIDED_VALUES_TABLE)?;
            let mut certificates = tx.open_table(CERTIFICATES_TABLE)?;
            let mut decided_block_data = tx.open_table(DECIDED_BLOCK_DATA_TABLE)?;

            let keys = self.height_range(&decided, ..retain_height)?;
            for key in &keys {
                decided.remove(key)?;
                certificates.remove(key)?;
                decided_block_data.remove(key)?;
            }

            keys
        };

        tx.commit()?;

        self.metrics.observe_delete_time(start.elapsed());

        Ok(pruned)
    }

    fn min_decided_value_height(&self) -> Option<Height> {
        let start = Instant::now();

        let tx = self.db.begin_read().unwrap();
        let table = tx.open_table(DECIDED_VALUES_TABLE).unwrap();
        let (key, value) = table.first().ok()??;

        self.metrics.observe_read_time(start.elapsed());
        self.metrics.add_read_bytes(value.value().len() as u64);
        self.metrics.add_key_read_bytes(size_of::<Height>() as u64);

        Some(key.value())
    }

    // fn max_decided_value_height(&self) -> Option<Height> {
    //     let tx = self.db.begin_read().unwrap();
    //     let table = tx.open_table(DECIDED_VALUES_TABLE).unwrap();
    //     let (key, _) = table.last().ok()??;
    //     Some(key.value())
    // }

    fn create_tables(&self) -> Result<(), StoreError> {
        let tx = self.db.begin_write()?;

        // Implicitly creates the tables if they do not exist yet
        let _ = tx.open_table(DECIDED_VALUES_TABLE)?;
        let _ = tx.open_table(CERTIFICATES_TABLE)?;
        let _ = tx.open_table(UNDECIDED_PROPOSALS_TABLE)?;
        let _ = tx.open_table(DECIDED_BLOCK_DATA_TABLE)?;
        let _ = tx.open_table(UNDECIDED_BLOCK_DATA_TABLE)?;
        let _ = tx.open_table(BLOCK_HEADERS_TABLE)?;

        // Phase 4: Three-layer architecture tables
        let _ = tx.open_table(CONSENSUS_BLOCK_METADATA_TABLE)?;
        let _ = tx.open_table(BLOB_METADATA_DECIDED_TABLE)?;
        let _ = tx.open_table(BLOB_METADATA_UNDECIDED_TABLE)?;
        let _ = tx.open_table(BLOB_METADATA_META_TABLE)?;

        tx.commit()?;

        Ok(())
    }

    fn get_block_data(&self, height: Height, round: Round) -> Result<Option<Bytes>, StoreError> {
        let start = Instant::now();

        let tx = self.db.begin_read()?;

        // Try undecided block data first
        let undecided_table = tx.open_table(UNDECIDED_BLOCK_DATA_TABLE)?;
        if let Some(data) = undecided_table.get(&(height, round))? {
            let bytes = data.value();
            let read_bytes = bytes.len() as u64;
            self.metrics.observe_read_time(start.elapsed());
            self.metrics.add_read_bytes(read_bytes);
            self.metrics.add_key_read_bytes((size_of::<Height>() + size_of::<Round>()) as u64);
            return Ok(Some(Bytes::copy_from_slice(&bytes)));
        }

        // Then try decided block data
        let decided_table = tx.open_table(DECIDED_BLOCK_DATA_TABLE)?;
        if let Some(data) = decided_table.get(&height)? {
            let bytes = data.value();
            let read_bytes = bytes.len() as u64;
            self.metrics.observe_read_time(start.elapsed());
            self.metrics.add_read_bytes(read_bytes);
            self.metrics.add_key_read_bytes(size_of::<Height>() as u64);
            return Ok(Some(Bytes::copy_from_slice(&bytes)));
        }

        self.metrics.observe_read_time(start.elapsed());
        Ok(None)
    }

    fn insert_undecided_block_data(
        &self,
        height: Height,
        round: Round,
        data: Bytes,
    ) -> Result<(), StoreError> {
        let start = Instant::now();
        let write_bytes = data.len() as u64;

        let tx = self.db.begin_write()?;
        {
            let mut table = tx.open_table(UNDECIDED_BLOCK_DATA_TABLE)?;
            let key = (height, round);
            // Only insert if no value exists at this key
            if table.get(&key)?.is_none() {
                table.insert(key, data.to_vec())?;
            }
        }
        tx.commit()?;

        self.metrics.observe_write_time(start.elapsed());
        self.metrics.add_write_bytes(write_bytes);

        Ok(())
    }

    fn insert_decided_block_data(&self, height: Height, data: Bytes) -> Result<(), StoreError> {
        let start = Instant::now();
        let write_bytes = data.len() as u64;

        let tx = self.db.begin_write()?;
        {
            let mut table = tx.open_table(DECIDED_BLOCK_DATA_TABLE)?;
            // Only insert if no value exists at this key
            if table.get(&height)?.is_none() {
                table.insert(height, data.to_vec())?;
            }
        }
        tx.commit()?;

        self.metrics.observe_write_time(start.elapsed());
        self.metrics.add_write_bytes(write_bytes);

        Ok(())
    }

    fn insert_block_header(
        &self,
        height: Height,
        header: &SignedBeaconBlockHeader,
    ) -> Result<(), StoreError> {
        let start = Instant::now();

        let proto = header.to_proto()?;
        let bytes = proto.encode_to_vec();
        let write_bytes = bytes.len() as u64;

        let tx = self.db.begin_write()?;
        {
            let mut table = tx.open_table(BLOCK_HEADERS_TABLE)?;
            table.insert(height, bytes)?;
        }
        tx.commit()?;

        self.metrics.observe_write_time(start.elapsed());
        self.metrics.add_write_bytes(write_bytes);

        Ok(())
    }

    fn get_block_header(
        &self,
        height: Height,
    ) -> Result<Option<SignedBeaconBlockHeader>, StoreError> {
        let start = Instant::now();
        let tx = self.db.begin_read()?;
        let table = tx.open_table(BLOCK_HEADERS_TABLE)?;

        let header = if let Some(value) = table.get(&height)? {
            let bytes = value.value();
            let mut buf = Bytes::copy_from_slice(&bytes);
            let proto = proto::SignedBeaconBlockHeader::decode(&mut buf)
                .map_err(|e| StoreError::Protobuf(ProtoError::Other(e.to_string())))?;
            let header = SignedBeaconBlockHeader::from_proto(proto)?;

            self.metrics.add_read_bytes(bytes.len() as u64);
            self.metrics.add_key_read_bytes(size_of::<Height>() as u64);

            Some(header)
        } else {
            None
        };

        self.metrics.observe_read_time(start.elapsed());

        Ok(header)
    }

    fn get_latest_block_header(
        &self,
    ) -> Result<Option<(Height, SignedBeaconBlockHeader)>, StoreError> {
        let start = Instant::now();
        let tx = self.db.begin_read()?;
        let table = tx.open_table(BLOCK_HEADERS_TABLE)?;

        let mut iter = table.iter()?;
        let mut result = None;

        if let Some(entry) = iter.next_back() {
            let (height_guard, value_guard) = entry?;
            let height = height_guard.value();
            let bytes = value_guard.value();
            let mut buf = Bytes::copy_from_slice(&bytes);
            let proto = proto::SignedBeaconBlockHeader::decode(&mut buf)
                .map_err(|e| StoreError::Protobuf(ProtoError::Other(e.to_string())))?;
            let header = SignedBeaconBlockHeader::from_proto(proto)?;

            self.metrics.add_read_bytes(bytes.len() as u64);
            self.metrics.add_key_read_bytes(size_of::<Height>() as u64);

            result = Some((height, header));
        }

        self.metrics.observe_read_time(start.elapsed());

        Ok(result)
    }

    // ========================================================================
    // Phase 4: Three-layer architecture - metadata storage methods
    // ========================================================================

    /// Insert consensus block metadata (Layer 1)
    ///
    /// This stores pure BFT consensus metadata using Tendermint/Malachite terminology.
    /// Idempotent: compares bytes before writing to avoid unnecessary disk I/O.
    fn insert_consensus_block_metadata(
        &self,
        height: Height,
        metadata: &ConsensusBlockMetadata,
    ) -> Result<(), StoreError> {
        let start = Instant::now();

        let proto = metadata.to_proto()?;
        let bytes = proto.encode_to_vec();
        let write_bytes = bytes.len() as u64;

        let tx = self.db.begin_write()?;
        {
            let mut table = tx.open_table(CONSENSUS_BLOCK_METADATA_TABLE)?;

            // Idempotent write: only insert if value doesn't exist or differs
            let should_write = if let Some(existing) = table.get(&height)? {
                existing.value() != bytes.as_slice()
            } else {
                true
            };

            if should_write {
                table.insert(height, bytes)?;
            }
        }
        tx.commit()?;

        self.metrics.observe_write_time(start.elapsed());
        self.metrics.add_write_bytes(write_bytes);

        Ok(())
    }

    /// Get consensus block metadata (Layer 1)
    fn get_consensus_block_metadata(
        &self,
        height: Height,
    ) -> Result<Option<ConsensusBlockMetadata>, StoreError> {
        let start = Instant::now();
        let tx = self.db.begin_read()?;
        let table = tx.open_table(CONSENSUS_BLOCK_METADATA_TABLE)?;

        let metadata = if let Some(value) = table.get(&height)? {
            let bytes = value.value();
            let mut buf = Bytes::copy_from_slice(&bytes);
            let proto = proto::ConsensusBlockMetadata::decode(&mut buf)
                .map_err(|e| StoreError::Protobuf(ProtoError::Other(e.to_string())))?;
            let metadata = ConsensusBlockMetadata::from_proto(proto)?;

            self.metrics.add_read_bytes(bytes.len() as u64);
            self.metrics.add_key_read_bytes(size_of::<Height>() as u64);

            Some(metadata)
        } else {
            None
        };

        self.metrics.observe_read_time(start.elapsed());

        Ok(metadata)
    }

    /// Insert undecided blob metadata (Layer 2)
    ///
    /// Stores blob metadata for a specific (height, round) proposal.
    /// Idempotent: compares bytes before writing.
    fn insert_blob_metadata_undecided(
        &self,
        height: Height,
        round: Round,
        metadata: &BlobMetadata,
    ) -> Result<(), StoreError> {
        let start = Instant::now();

        let proto = metadata.to_proto()?;
        let bytes = proto.encode_to_vec();
        let write_bytes = bytes.len() as u64;

        let key = (height, round);
        let tx = self.db.begin_write()?;
        {
            let mut table = tx.open_table(BLOB_METADATA_UNDECIDED_TABLE)?;

            // Idempotent write: only insert if value doesn't exist or differs
            let should_write = if let Some(existing) = table.get(&key)? {
                existing.value() != bytes.as_slice()
            } else {
                true
            };

            if should_write {
                table.insert(key, bytes)?;
            }
        }
        tx.commit()?;

        self.metrics.observe_write_time(start.elapsed());
        self.metrics.add_write_bytes(write_bytes);

        Ok(())
    }

    /// Get undecided blob metadata (Layer 2)
    fn get_blob_metadata_undecided(
        &self,
        height: Height,
        round: Round,
    ) -> Result<Option<BlobMetadata>, StoreError> {
        let start = Instant::now();
        let tx = self.db.begin_read()?;
        let table = tx.open_table(BLOB_METADATA_UNDECIDED_TABLE)?;

        let key = (height, round);
        let metadata = if let Some(value) = table.get(&key)? {
            let bytes = value.value();
            let mut buf = Bytes::copy_from_slice(&bytes);
            let proto = proto::BlobMetadata::decode(&mut buf)
                .map_err(|e| StoreError::Protobuf(ProtoError::Other(e.to_string())))?;
            let metadata = BlobMetadata::from_proto(proto)?;

            self.metrics.add_read_bytes(bytes.len() as u64);
            self.metrics.add_key_read_bytes(size_of::<(Height, Round)>() as u64);

            Some(metadata)
        } else {
            None
        };

        self.metrics.observe_read_time(start.elapsed());

        Ok(metadata)
    }

    /// Get decided blob metadata (Layer 2)
    fn get_blob_metadata(&self, height: Height) -> Result<Option<BlobMetadata>, StoreError> {
        let start = Instant::now();
        let tx = self.db.begin_read()?;
        let table = tx.open_table(BLOB_METADATA_DECIDED_TABLE)?;

        let metadata = if let Some(value) = table.get(&height)? {
            let bytes = value.value();
            let mut buf = Bytes::copy_from_slice(&bytes);
            let proto = proto::BlobMetadata::decode(&mut buf)
                .map_err(|e| StoreError::Protobuf(ProtoError::Other(e.to_string())))?;
            let metadata = BlobMetadata::from_proto(proto)?;

            self.metrics.add_read_bytes(bytes.len() as u64);
            self.metrics.add_key_read_bytes(size_of::<Height>() as u64);

            Some(metadata)
        } else {
            None
        };

        self.metrics.observe_read_time(start.elapsed());

        Ok(metadata)
    }

    /// Mark blob metadata as decided (atomic promotion)
    ///
    /// This is the CRITICAL method that ensures atomicity:
    /// 1. Reads from undecided table
    /// 2. Writes to decided table
    /// 3. Updates latest metadata pointer
    /// 4. Deletes from undecided table
    ///
    /// All operations happen in a single WriteBatch transaction.
    fn mark_blob_metadata_decided(&self, height: Height, round: Round) -> Result<(), StoreError> {
        let start = Instant::now();
        let mut write_bytes = 0;

        let key = (height, round);
        let tx = self.db.begin_write()?;

        // 1. Read from undecided table
        let metadata_bytes = {
            let table = tx.open_table(BLOB_METADATA_UNDECIDED_TABLE)?;
            if let Some(value) = table.get(&key)? {
                let bytes = value.value().to_vec();
                write_bytes += bytes.len() as u64;
                bytes
            } else {
                // If not in undecided, check if already decided (idempotent)
                let decided_table = tx.open_table(BLOB_METADATA_DECIDED_TABLE)?;
                if decided_table.get(&height)?.is_some() {
                    // Already decided, this is idempotent - just return success
                    return Ok(());
                } else {
                    return Err(StoreError::MissingBlobMetadata {
                        height: height.as_u64(),
                        round: round.as_i64(),
                    });
                }
            }
        };

        // 2. Write to decided table
        {
            let mut table = tx.open_table(BLOB_METADATA_DECIDED_TABLE)?;
            table.insert(height, metadata_bytes.clone())?;
        }

        // 3. Update latest metadata pointer
        {
            let mut meta_table = tx.open_table(BLOB_METADATA_META_TABLE)?;
            let height_bytes = height.as_u64().to_be_bytes().to_vec();
            meta_table.insert("latest_height", height_bytes)?;
        }

        // 4. Delete from undecided table
        {
            let mut table = tx.open_table(BLOB_METADATA_UNDECIDED_TABLE)?;
            table.remove(&key)?;
        }

        tx.commit()?;

        self.metrics.observe_write_time(start.elapsed());
        self.metrics.add_write_bytes(write_bytes);

        Ok(())
    }

    /// Get latest blob metadata (O(1) via metadata pointer)
    fn get_latest_blob_metadata(&self) -> Result<Option<(Height, BlobMetadata)>, StoreError> {
        let start = Instant::now();
        let tx = self.db.begin_read()?;

        // Read latest height from metadata pointer
        let latest_height = {
            let meta_table = tx.open_table(BLOB_METADATA_META_TABLE)?;
            if let Some(value) = meta_table.get("latest_height")? {
                let bytes = value.value();
                if bytes.len() == 8 {
                    let height_u64 = u64::from_be_bytes(bytes.try_into().unwrap());
                    Some(Height::new(height_u64))
                } else {
                    None
                }
            } else {
                None
            }
        };

        // If we have a latest height, fetch the metadata
        let result = if let Some(height) = latest_height {
            let table = tx.open_table(BLOB_METADATA_DECIDED_TABLE)?;
            if let Some(value) = table.get(&height)? {
                let bytes = value.value();
                let mut buf = Bytes::copy_from_slice(&bytes);
                let proto = proto::BlobMetadata::decode(&mut buf)
                    .map_err(|e| StoreError::Protobuf(ProtoError::Other(e.to_string())))?;
                let metadata = BlobMetadata::from_proto(proto)?;

                self.metrics.add_read_bytes(bytes.len() as u64);
                self.metrics.add_key_read_bytes(size_of::<Height>() as u64);

                Some((height, metadata))
            } else {
                None
            }
        } else {
            None
        };

        self.metrics.observe_read_time(start.elapsed());

        Ok(result)
    }

    /// Get all undecided blob metadata entries before a given height
    ///
    /// Used for startup cleanup to remove stale entries.
    fn get_all_undecided_blob_metadata_before(
        &self,
        before_height: Height,
    ) -> Result<Vec<(Height, Round)>, StoreError> {
        let start = Instant::now();
        let tx = self.db.begin_read()?;
        let table = tx.open_table(BLOB_METADATA_UNDECIDED_TABLE)?;

        let mut keys = Vec::new();
        for entry in table.iter()? {
            let (key_guard, _) = entry?;
            let (height, round) = key_guard.value();
            if height >= before_height {
                break;
            }
            keys.push((height, round));
        }

        self.metrics.observe_read_time(start.elapsed());

        Ok(keys)
    }

    /// Delete undecided blob metadata entry
    ///
    /// Used during cleanup.
    fn delete_blob_metadata_undecided(
        &self,
        height: Height,
        round: Round,
    ) -> Result<(), StoreError> {
        let start = Instant::now();

        let key = (height, round);
        let tx = self.db.begin_write()?;
        {
            let mut table = tx.open_table(BLOB_METADATA_UNDECIDED_TABLE)?;
            table.remove(&key)?;
        }
        tx.commit()?;

        self.metrics.observe_write_time(start.elapsed());

        Ok(())
    }
}

#[derive(Clone)]
pub struct Store {
    db: Arc<Db>,
}

impl Store {
    pub fn open(path: impl AsRef<Path>, metrics: DbMetrics) -> Result<Self, StoreError> {
        let db = Db::new(path, metrics)?;
        db.create_tables()?;

        Ok(Self { db: Arc::new(db) })
    }

    pub async fn min_decided_value_height(&self) -> Option<Height> {
        let db = Arc::clone(&self.db);
        tokio::task::spawn_blocking(move || db.min_decided_value_height()).await.ok().flatten()
    }

    // pub async fn max_decided_value_height(&self) -> Option<Height> {
    //     let db = Arc::clone(&self.db);
    //     tokio::task::spawn_blocking(move || db.max_decided_value_height())
    //         .await
    //         .ok()
    //         .flatten()
    // }

    pub async fn get_decided_value(
        &self,
        height: Height,
    ) -> Result<Option<DecidedValue>, StoreError> {
        let db = Arc::clone(&self.db);

        tokio::task::spawn_blocking(move || db.get_decided_value(height)).await?
    }

    pub async fn store_decided_value(
        &self,
        certificate: &CommitCertificate<LoadContext>,
        value: Value,
    ) -> Result<(), StoreError> {
        let decided_value = DecidedValue { value, certificate: certificate.clone() };

        let db = Arc::clone(&self.db);
        tokio::task::spawn_blocking(move || db.insert_decided_value(decided_value)).await?
    }

    pub async fn store_undecided_proposal(
        &self,
        value: ProposedValue<LoadContext>,
    ) -> Result<(), StoreError> {
        let db = Arc::clone(&self.db);
        tokio::task::spawn_blocking(move || db.insert_undecided_proposal(value)).await?
    }

    pub async fn get_undecided_proposal(
        &self,
        height: Height,
        round: Round,
    ) -> Result<Option<ProposedValue<LoadContext>>, StoreError> {
        let db = Arc::clone(&self.db);
        tokio::task::spawn_blocking(move || db.get_undecided_proposal(height, round)).await?
    }

    pub async fn prune(&self, retain_height: Height) -> Result<Vec<Height>, StoreError> {
        let db = Arc::clone(&self.db);
        tokio::task::spawn_blocking(move || db.prune(retain_height)).await?
    }

    pub async fn put_blob_sidecar_header(
        &self,
        height: Height,
        header: &SignedBeaconBlockHeader,
    ) -> Result<(), StoreError> {
        let db = Arc::clone(&self.db);
        let header = header.clone();
        tokio::task::spawn_blocking(move || db.insert_block_header(height, &header)).await?
    }

    pub async fn get_blob_sidecar_header(
        &self,
        height: Height,
    ) -> Result<Option<SignedBeaconBlockHeader>, StoreError> {
        let db = Arc::clone(&self.db);
        tokio::task::spawn_blocking(move || db.get_block_header(height)).await?
    }

    pub async fn get_latest_blob_sidecar_header(
        &self,
    ) -> Result<Option<(Height, SignedBeaconBlockHeader)>, StoreError> {
        let db = Arc::clone(&self.db);
        tokio::task::spawn_blocking(move || db.get_latest_block_header()).await?
    }

    pub async fn get_block_data(
        &self,
        height: Height,
        round: Round,
    ) -> Result<Option<Bytes>, StoreError> {
        let db = Arc::clone(&self.db);
        tokio::task::spawn_blocking(move || db.get_block_data(height, round)).await?
    }

    pub async fn store_undecided_block_data(
        &self,
        height: Height,
        round: Round,
        data: Bytes,
    ) -> Result<(), StoreError> {
        let db = Arc::clone(&self.db);
        tokio::task::spawn_blocking(move || db.insert_undecided_block_data(height, round, data))
            .await?
    }

    pub async fn store_decided_block_data(
        &self,
        height: Height,
        data: Bytes,
    ) -> Result<(), StoreError> {
        let db = Arc::clone(&self.db);
        tokio::task::spawn_blocking(move || db.insert_decided_block_data(height, data)).await?
    }

    // ========================================================================
    // Phase 4: Three-layer architecture - Store async wrappers
    // ========================================================================

    /// Insert consensus block metadata (Layer 1)
    pub async fn put_consensus_block_metadata(
        &self,
        height: Height,
        metadata: &ConsensusBlockMetadata,
    ) -> Result<(), StoreError> {
        let db = Arc::clone(&self.db);
        let metadata = metadata.clone();
        tokio::task::spawn_blocking(move || db.insert_consensus_block_metadata(height, &metadata))
            .await?
    }

    /// Get consensus block metadata (Layer 1)
    pub async fn get_consensus_block_metadata(
        &self,
        height: Height,
    ) -> Result<Option<ConsensusBlockMetadata>, StoreError> {
        let db = Arc::clone(&self.db);
        tokio::task::spawn_blocking(move || db.get_consensus_block_metadata(height)).await?
    }

    /// Insert undecided blob metadata (Layer 2)
    pub async fn put_blob_metadata_undecided(
        &self,
        height: Height,
        round: Round,
        metadata: &BlobMetadata,
    ) -> Result<(), StoreError> {
        let db = Arc::clone(&self.db);
        let metadata = metadata.clone();
        tokio::task::spawn_blocking(move || {
            db.insert_blob_metadata_undecided(height, round, &metadata)
        })
        .await?
    }

    /// Get undecided blob metadata (Layer 2)
    pub async fn get_blob_metadata_undecided(
        &self,
        height: Height,
        round: Round,
    ) -> Result<Option<BlobMetadata>, StoreError> {
        let db = Arc::clone(&self.db);
        tokio::task::spawn_blocking(move || db.get_blob_metadata_undecided(height, round)).await?
    }

    /// Get decided blob metadata (Layer 2)
    pub async fn get_blob_metadata(
        &self,
        height: Height,
    ) -> Result<Option<BlobMetadata>, StoreError> {
        let db = Arc::clone(&self.db);
        tokio::task::spawn_blocking(move || db.get_blob_metadata(height)).await?
    }

    /// Mark blob metadata as decided (atomic promotion)
    ///
    /// This is the critical method for committing metadata:
    /// - Reads from undecided table
    /// - Writes to decided table
    /// - Updates latest pointer
    /// - Deletes from undecided
    ///
    /// All in a single atomic transaction.
    pub async fn mark_blob_metadata_decided(
        &self,
        height: Height,
        round: Round,
    ) -> Result<(), StoreError> {
        let db = Arc::clone(&self.db);
        tokio::task::spawn_blocking(move || db.mark_blob_metadata_decided(height, round)).await?
    }

    /// Get latest blob metadata (O(1) via metadata pointer)
    pub async fn get_latest_blob_metadata(
        &self,
    ) -> Result<Option<(Height, BlobMetadata)>, StoreError> {
        let db = Arc::clone(&self.db);
        tokio::task::spawn_blocking(move || db.get_latest_blob_metadata()).await?
    }

    /// Get all undecided blob metadata before a given height
    ///
    /// Used for startup cleanup.
    pub async fn get_all_undecided_blob_metadata_before(
        &self,
        before_height: Height,
    ) -> Result<Vec<(Height, Round)>, StoreError> {
        let db = Arc::clone(&self.db);
        tokio::task::spawn_blocking(move || {
            db.get_all_undecided_blob_metadata_before(before_height)
        })
        .await?
    }

    /// Delete undecided blob metadata
    ///
    /// Used during cleanup.
    pub async fn delete_blob_metadata_undecided(
        &self,
        height: Height,
        round: Round,
    ) -> Result<(), StoreError> {
        let db = Arc::clone(&self.db);
        tokio::task::spawn_blocking(move || db.delete_blob_metadata_undecided(height, round))
            .await?
    }
}

#[cfg(test)]
mod tests {
    use malachitebft_app_channel::app::types::core::Round;
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn mark_blob_metadata_decided_without_entry_returns_missing_error() {
        let tmp = tempdir().expect("tempdir");
        let db_path = tmp.path().join("store.redb");
        let db = Db::new(&db_path, DbMetrics::new()).expect("db");
        db.create_tables().expect("tables");

        let height = Height::new(1);
        let round = Round::new(0);

        let err = db.mark_blob_metadata_decided(height, round).expect_err("expected error");
        match err {
            StoreError::MissingBlobMetadata { height: h, round: r } => {
                assert_eq!(h, height.as_u64());
                assert_eq!(r, round.as_i64());
            }
            other => panic!("unexpected error variant: {other:?}"),
        }
    }
}
