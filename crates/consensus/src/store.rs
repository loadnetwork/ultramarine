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
    aliases::Bytes as AlloyBytes,
    archive::ArchiveRecord,
    blob_metadata::BlobMetadata,
    codec::{proto as codec, proto::ProtobufCodec},
    consensus_block_metadata::ConsensusBlockMetadata,
    context::LoadContext,
    height::Height,
    proto,
    value::Value,
};

#[derive(Clone, Debug)]
struct BlockPayloadRecord {
    execution_payload_ssz: Bytes,
    execution_requests: Vec<AlloyBytes>,
}

impl BlockPayloadRecord {
    fn new(execution_payload_ssz: Bytes, execution_requests: Vec<AlloyBytes>) -> Self {
        Self { execution_payload_ssz, execution_requests }
    }

    fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        let proto_record = proto::BlockPayloadRecord {
            execution_payload_ssz: self.execution_payload_ssz.clone(),
            execution_requests: self.execution_requests.iter().cloned().map(|req| req.0).collect(),
        };
        Ok(proto_record.encode_to_vec())
    }

    fn decode(bytes: &[u8]) -> Result<Self, ProtoError> {
        match proto::BlockPayloadRecord::decode(bytes) {
            Ok(record) => Ok(Self {
                execution_payload_ssz: record.execution_payload_ssz,
                execution_requests: record
                    .execution_requests
                    .into_iter()
                    .map(AlloyBytes::from)
                    .collect(),
            }),
            Err(_) => Ok(Self {
                execution_payload_ssz: Bytes::copy_from_slice(bytes),
                execution_requests: Vec::new(),
            }),
        }
    }
}

mod keys;
use keys::{BlobArchivalKey, HeightKey, UndecidedValueKey};

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

// Archive notices persisted per (height, blob_index)
const BLOB_ARCHIVAL_TABLE: redb::TableDefinition<BlobArchivalKey, Vec<u8>> =
    redb::TableDefinition::new("blob_archival");

struct Db {
    db: redb::Database,
    metrics: DbMetrics,
}

impl Db {
    fn new(path: impl AsRef<Path>, metrics: DbMetrics) -> Result<Self, StoreError> {
        let db = redb::Database::create(path).map_err(StoreError::Database)?;
        Ok(Self { db, metrics })
    }

    fn open_existing(path: impl AsRef<Path>, metrics: DbMetrics) -> Result<Self, StoreError> {
        let db = redb::Database::open(path).map_err(StoreError::Database)?;
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

    fn delete_undecided_proposal(&self, height: Height, round: Round) -> Result<(), StoreError> {
        let start = Instant::now();

        let key = (height, round);
        let tx = self.db.begin_write()?;
        {
            let mut table = tx.open_table(UNDECIDED_PROPOSALS_TABLE)?;
            table.remove(&key)?;
        }
        tx.commit()?;

        self.metrics.observe_write_time(start.elapsed());

        Ok(())
    }

    // NOTE: height_range() was removed as part of FIX-001.
    // It was only used for pruning decided data, which we no longer do.
    // See store.prune() documentation for details.

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

    /// Prune undecided data (temporary proposals from failed rounds).
    ///
    /// NOTE: This function intentionally does NOT delete decided data
    /// (DECIDED_VALUES_TABLE, CERTIFICATES_TABLE, DECIDED_BLOCK_DATA_TABLE).
    /// Following the Lighthouse pattern, only blob bytes are pruned after archival.
    /// Decided values, certificates, and block data must be retained forever
    /// to allow fullnodes to sync the complete chain history.
    ///
    /// Load Network context: Validators prune blob bytes via blob_engine after
    /// archival to S3, but must serve historical block data for ValueSync.
    fn prune(&self, retain_height: Height) -> Result<Vec<Height>, StoreError> {
        let start = Instant::now();

        let tx = self.db.begin_write()?;

        {
            // Only prune undecided proposals (temp data from failed consensus rounds)
            let mut undecided = tx.open_table(UNDECIDED_PROPOSALS_TABLE)?;
            let keys = self.undecided_proposals_range(&undecided, ..(retain_height, Round::Nil))?;
            for key in keys {
                undecided.remove(key)?;
            }

            // Only prune undecided block data (temp data from failed consensus rounds)
            let mut undecided_block_data = tx.open_table(UNDECIDED_BLOCK_DATA_TABLE)?;
            let keys =
                self.block_data_range(&undecided_block_data, ..(retain_height, Round::Nil))?;
            for key in &keys {
                undecided_block_data.remove(key)?;
            }

            // DO NOT touch: DECIDED_VALUES_TABLE, CERTIFICATES_TABLE, DECIDED_BLOCK_DATA_TABLE
            // These are historical records required for fullnode sync and must be retained forever.
            // Blob bytes are pruned separately via blob_engine.mark_archived() after S3 archival.
        }

        tx.commit()?;

        self.metrics.observe_delete_time(start.elapsed());

        // Return empty vec - we no longer prune decided heights
        Ok(vec![])
    }

    fn min_decided_value_height(&self) -> Option<Height> {
        let start = Instant::now();

        let tx = self.db.begin_read().ok()?;
        let table = tx.open_table(DECIDED_VALUES_TABLE).ok()?;
        let (key, value) = table.first().ok()??;

        self.metrics.observe_read_time(start.elapsed());
        self.metrics.add_read_bytes(value.value().len() as u64);
        self.metrics.add_key_read_bytes(size_of::<Height>() as u64);

        Some(key.value())
    }

    fn max_decided_value_height(&self) -> Option<Height> {
        let tx = self.db.begin_read().ok()?;
        let table = tx.open_table(DECIDED_VALUES_TABLE).ok()?;
        let (key, _) = table.last().ok()??;
        Some(key.value())
    }

    fn create_tables(&self) -> Result<(), StoreError> {
        let tx = self.db.begin_write()?;

        // Implicitly creates the tables if they do not exist yet
        let _ = tx.open_table(DECIDED_VALUES_TABLE)?;
        let _ = tx.open_table(CERTIFICATES_TABLE)?;
        let _ = tx.open_table(UNDECIDED_PROPOSALS_TABLE)?;
        let _ = tx.open_table(DECIDED_BLOCK_DATA_TABLE)?;
        let _ = tx.open_table(UNDECIDED_BLOCK_DATA_TABLE)?;

        // Phase 4: Three-layer architecture tables
        let _ = tx.open_table(CONSENSUS_BLOCK_METADATA_TABLE)?;
        let _ = tx.open_table(BLOB_METADATA_DECIDED_TABLE)?;
        let _ = tx.open_table(BLOB_METADATA_UNDECIDED_TABLE)?;
        let _ = tx.open_table(BLOB_METADATA_META_TABLE)?;
        let _ = tx.open_table(BLOB_ARCHIVAL_TABLE)?;

        tx.commit()?;

        Ok(())
    }

    fn load_block_payload_record(
        &self,
        height: Height,
        round: Round,
    ) -> Result<Option<BlockPayloadRecord>, StoreError> {
        let start = Instant::now();

        let tx = self.db.begin_read()?;

        // Try undecided block data first
        let undecided_table = tx.open_table(UNDECIDED_BLOCK_DATA_TABLE)?;
        if let Some(data) = undecided_table.get(&(height, round))? {
            let bytes = data.value();
            let read_bytes = bytes.len() as u64;
            let record = BlockPayloadRecord::decode(&bytes)?;
            self.metrics.observe_read_time(start.elapsed());
            self.metrics.add_read_bytes(read_bytes);
            self.metrics.add_key_read_bytes((size_of::<Height>() + size_of::<Round>()) as u64);
            return Ok(Some(record));
        }

        // Then try decided block data
        let decided_table = tx.open_table(DECIDED_BLOCK_DATA_TABLE)?;
        if let Some(data) = decided_table.get(&height)? {
            let bytes = data.value();
            let read_bytes = bytes.len() as u64;
            let record = BlockPayloadRecord::decode(&bytes)?;
            self.metrics.observe_read_time(start.elapsed());
            self.metrics.add_read_bytes(read_bytes);
            self.metrics.add_key_read_bytes(size_of::<Height>() as u64);
            return Ok(Some(record));
        }

        self.metrics.observe_read_time(start.elapsed());
        Ok(None)
    }

    fn get_block_data(&self, height: Height, round: Round) -> Result<Option<Bytes>, StoreError> {
        self.load_block_payload_record(height, round)
            .map(|opt| opt.map(|record| record.execution_payload_ssz))
    }

    fn get_execution_requests(
        &self,
        height: Height,
        round: Round,
    ) -> Result<Option<Vec<AlloyBytes>>, StoreError> {
        self.load_block_payload_record(height, round)
            .map(|opt| opt.map(|record| record.execution_requests))
    }

    fn insert_undecided_block_data(
        &self,
        height: Height,
        round: Round,
        record: BlockPayloadRecord,
    ) -> Result<(), StoreError> {
        let start = Instant::now();
        let encoded = record.encode().map_err(StoreError::Protobuf)?;
        let write_bytes = encoded.len() as u64;

        let tx = self.db.begin_write()?;
        {
            let mut table = tx.open_table(UNDECIDED_BLOCK_DATA_TABLE)?;
            let key = (height, round);
            if table.get(&key)?.is_none() {
                table.insert(key, encoded)?;
            }
        }
        tx.commit()?;

        self.metrics.observe_write_time(start.elapsed());
        self.metrics.add_write_bytes(write_bytes);

        Ok(())
    }

    fn delete_undecided_block_data(&self, height: Height, round: Round) -> Result<(), StoreError> {
        let start = Instant::now();

        let key = (height, round);
        let tx = self.db.begin_write()?;
        {
            let mut table = tx.open_table(UNDECIDED_BLOCK_DATA_TABLE)?;
            table.remove(&key)?;
        }
        tx.commit()?;

        self.metrics.observe_write_time(start.elapsed());

        Ok(())
    }

    fn insert_decided_block_data(
        &self,
        height: Height,
        record: BlockPayloadRecord,
    ) -> Result<(), StoreError> {
        let start = Instant::now();
        let encoded = record.encode().map_err(StoreError::Protobuf)?;
        let write_bytes = encoded.len() as u64;

        let tx = self.db.begin_write()?;
        {
            let mut table = tx.open_table(DECIDED_BLOCK_DATA_TABLE)?;
            if table.get(&height)?.is_none() {
                table.insert(height, encoded)?;
            }
        }
        tx.commit()?;

        self.metrics.observe_write_time(start.elapsed());
        self.metrics.add_write_bytes(write_bytes);

        Ok(())
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

            // BUG-014 fix: Only insert if no value exists at this key.
            // Previous "overwrite if different" guard allowed WAL replay race
            // conditions to corrupt blob metadata when the proposer rebuilds a
            // block with a different timestamp during crash recovery.
            // This now matches insert_undecided_block_data and
            // insert_undecided_proposal which both use is_none() guards.
            if table.get(&key)?.is_none() {
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
            let mut metadata = BlobMetadata::from_proto(proto)?;
            self.hydrate_archival_records(&tx, height, &mut metadata)?;

            self.metrics.add_read_bytes(bytes.len() as u64);
            self.metrics.add_key_read_bytes(size_of::<Height>() as u64);

            Some(metadata)
        } else {
            None
        };

        self.metrics.observe_read_time(start.elapsed());

        Ok(metadata)
    }

    fn insert_archive_record(
        &self,
        height: Height,
        blob_index: u16,
        record: &ArchiveRecord,
    ) -> Result<(), StoreError> {
        let start = Instant::now();
        let record_proto = record.to_proto()?;
        let bytes = record_proto.encode_to_vec();

        let tx = self.db.begin_write()?;
        {
            let mut table = tx.open_table(BLOB_ARCHIVAL_TABLE)?;
            table.insert((height, blob_index), bytes)?;
        }
        tx.commit()?;

        self.metrics.observe_write_time(start.elapsed());

        Ok(())
    }

    fn archived_blob_indexes(&self, height: Height) -> Result<Vec<u16>, StoreError> {
        let tx = self.db.begin_read()?;
        let table = tx.open_table(BLOB_ARCHIVAL_TABLE)?;
        let mut indexes = Vec::new();
        for entry in table.range((height, 0u16)..=(height, u16::MAX))? {
            let (key, _) = entry?;
            let (_, idx) = key.value();
            indexes.push(idx);
        }
        Ok(indexes)
    }

    fn hydrate_archival_records(
        &self,
        tx: &redb::ReadTransaction,
        height: Height,
        metadata: &mut BlobMetadata,
    ) -> Result<(), StoreError> {
        let table = tx.open_table(BLOB_ARCHIVAL_TABLE)?;
        for entry in table.range((height, 0u16)..=(height, u16::MAX))? {
            let (_, value) = entry?;
            let bytes = value.value();
            let mut buf = Bytes::copy_from_slice(&bytes);
            let proto = proto::ArchiveRecord::decode(&mut buf)
                .map_err(|e| StoreError::Protobuf(ProtoError::Other(e.to_string())))?;
            let record = ArchiveRecord::from_proto(proto)?;
            metadata.set_archive_record(record);
        }
        Ok(())
    }

    fn archive_records(&self, height: Height) -> Result<Vec<ArchiveRecord>, StoreError> {
        let tx = self.db.begin_read()?;
        let table = tx.open_table(BLOB_ARCHIVAL_TABLE)?;
        let mut records = Vec::new();
        for entry in table.range((height, 0u16)..=(height, u16::MAX))? {
            let (_, value) = entry?;
            let bytes = value.value();
            let mut buf = Bytes::copy_from_slice(&bytes);
            let proto = proto::ArchiveRecord::decode(&mut buf)
                .map_err(|e| StoreError::Protobuf(ProtoError::Other(e.to_string())))?;
            records.push(ArchiveRecord::from_proto(proto)?);
        }
        Ok(records)
    }

    fn update_blob_metadata(
        &self,
        height: Height,
        metadata: &BlobMetadata,
    ) -> Result<(), StoreError> {
        let start = Instant::now();
        let proto = metadata.to_proto()?;
        let bytes = proto.encode_to_vec();

        let tx = self.db.begin_write()?;
        {
            let mut table = tx.open_table(BLOB_METADATA_DECIDED_TABLE)?;
            table.insert(height, bytes)?;
        }
        tx.commit()?;

        self.metrics.observe_write_time(start.elapsed());

        Ok(())
    }

    fn decided_heights(&self) -> Result<Vec<Height>, StoreError> {
        let tx = self.db.begin_read()?;
        let table = tx.open_table(BLOB_METADATA_DECIDED_TABLE)?;
        let mut heights = Vec::new();
        for entry in table.iter()? {
            let (key, _) = entry?;
            heights.push(key.value());
        }
        Ok(heights)
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

    /// Insert genesis blob metadata directly into decided table
    ///
    /// Seeds the blob metadata store with height 0 entry. This is called during
    /// bootstrap when the store is empty to satisfy the parent lookup requirement
    /// for the first blobbed proposal at height 1.
    ///
    /// # Arguments
    ///
    /// * `metadata` - Genesis BlobMetadata (height 0, empty commitments)
    fn insert_genesis_blob_metadata(&self, metadata: &BlobMetadata) -> Result<(), StoreError> {
        let start = Instant::now();

        let height = Height::new(0);
        let tx = self.db.begin_write()?;

        // Check if already exists (idempotent)
        {
            let table = tx.open_table(BLOB_METADATA_DECIDED_TABLE)?;
            if table.get(&height)?.is_some() {
                return Ok(()); // Already seeded, nothing to do
            }
        }

        // Serialize metadata
        let proto = metadata.to_proto()?;
        let metadata_bytes = proto.encode_to_vec();
        let write_bytes = metadata_bytes.len() as u64;

        // Insert into decided table
        {
            let mut table = tx.open_table(BLOB_METADATA_DECIDED_TABLE)?;
            table.insert(height, metadata_bytes)?;
        }

        // Update latest metadata pointer
        {
            let mut meta_table = tx.open_table(BLOB_METADATA_META_TABLE)?;
            let height_bytes = height.as_u64().to_be_bytes().to_vec();
            meta_table.insert("latest_height", height_bytes)?;
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
                let mut metadata = BlobMetadata::from_proto(proto)?;
                self.hydrate_archival_records(&tx, height, &mut metadata)?;

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

    /// Open an existing store without creating or mutating any tables.
    ///
    /// This is intended for read-only consumers (tests, diagnostics) that want to
    /// avoid taking write locks at open time.
    pub fn open_read_only(path: impl AsRef<Path>, metrics: DbMetrics) -> Result<Self, StoreError> {
        let db = Db::open_existing(path, metrics)?;
        Ok(Self { db: Arc::new(db) })
    }

    pub async fn min_decided_value_height(&self) -> Option<Height> {
        let db = Arc::clone(&self.db);
        tokio::task::spawn_blocking(move || db.min_decided_value_height()).await.ok().flatten()
    }

    pub async fn max_decided_value_height(&self) -> Option<Height> {
        let db = Arc::clone(&self.db);
        tokio::task::spawn_blocking(move || db.max_decided_value_height()).await.ok().flatten()
    }

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

    pub async fn delete_undecided_proposal(
        &self,
        height: Height,
        round: Round,
    ) -> Result<(), StoreError> {
        let db = Arc::clone(&self.db);
        tokio::task::spawn_blocking(move || db.delete_undecided_proposal(height, round)).await?
    }

    pub async fn prune(&self, retain_height: Height) -> Result<Vec<Height>, StoreError> {
        let db = Arc::clone(&self.db);
        tokio::task::spawn_blocking(move || db.prune(retain_height)).await?
    }

    pub async fn get_block_data(
        &self,
        height: Height,
        round: Round,
    ) -> Result<Option<Bytes>, StoreError> {
        let db = Arc::clone(&self.db);
        tokio::task::spawn_blocking(move || db.get_block_data(height, round)).await?
    }

    pub async fn get_execution_requests(
        &self,
        height: Height,
        round: Round,
    ) -> Result<Option<Vec<AlloyBytes>>, StoreError> {
        let db = Arc::clone(&self.db);
        tokio::task::spawn_blocking(move || db.get_execution_requests(height, round)).await?
    }

    pub async fn store_undecided_block_data(
        &self,
        height: Height,
        round: Round,
        data: Bytes,
        execution_requests: Vec<AlloyBytes>,
    ) -> Result<(), StoreError> {
        let db = Arc::clone(&self.db);
        tokio::task::spawn_blocking(move || {
            db.insert_undecided_block_data(
                height,
                round,
                BlockPayloadRecord::new(data, execution_requests),
            )
        })
        .await?
    }

    pub async fn delete_undecided_block_data(
        &self,
        height: Height,
        round: Round,
    ) -> Result<(), StoreError> {
        let db = Arc::clone(&self.db);
        tokio::task::spawn_blocking(move || db.delete_undecided_block_data(height, round)).await?
    }

    pub async fn store_decided_block_data(
        &self,
        height: Height,
        data: Bytes,
        execution_requests: Vec<AlloyBytes>,
    ) -> Result<(), StoreError> {
        let db = Arc::clone(&self.db);
        tokio::task::spawn_blocking(move || {
            db.insert_decided_block_data(height, BlockPayloadRecord::new(data, execution_requests))
        })
        .await?
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

    pub async fn insert_archive_record(
        &self,
        height: Height,
        blob_index: u16,
        record: ArchiveRecord,
    ) -> Result<(), StoreError> {
        let db = Arc::clone(&self.db);
        tokio::task::spawn_blocking(move || db.insert_archive_record(height, blob_index, &record))
            .await?
    }

    pub async fn archived_blob_indexes(&self, height: Height) -> Result<Vec<u16>, StoreError> {
        let db = Arc::clone(&self.db);
        tokio::task::spawn_blocking(move || db.archived_blob_indexes(height)).await?
    }

    pub async fn archive_records(&self, height: Height) -> Result<Vec<ArchiveRecord>, StoreError> {
        let db = Arc::clone(&self.db);
        tokio::task::spawn_blocking(move || db.archive_records(height)).await?
    }

    pub async fn update_blob_metadata(
        &self,
        height: Height,
        metadata: BlobMetadata,
    ) -> Result<(), StoreError> {
        let db = Arc::clone(&self.db);
        tokio::task::spawn_blocking(move || db.update_blob_metadata(height, &metadata)).await?
    }

    pub async fn decided_heights(&self) -> Result<Vec<Height>, StoreError> {
        let db = Arc::clone(&self.db);
        tokio::task::spawn_blocking(move || db.decided_heights()).await?
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

    /// Seed genesis blob metadata (height 0) into the store
    ///
    /// Called during bootstrap when the store is empty to satisfy the parent lookup
    /// requirement for the first blobbed proposal at height 1. Idempotent - safe to
    /// call multiple times.
    pub async fn seed_genesis_blob_metadata(&self) -> Result<(), StoreError> {
        let genesis_metadata = BlobMetadata::genesis();
        let db = Arc::clone(&self.db);
        tokio::task::spawn_blocking(move || db.insert_genesis_blob_metadata(&genesis_metadata))
            .await?
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
    use alloy_primitives::{Bloom, Bytes as AlloyBytes, U256};
    use malachitebft_app_channel::app::types::core::Round;
    use tempfile::tempdir;
    use ultramarine_types::{
        address::Address,
        aliases::B256,
        blob::{KzgCommitment, MAX_BLOBS_PER_BLOCK_ELECTRA},
        consensus_block_metadata::ConsensusBlockMetadata,
        constants::LOAD_EXECUTION_GAS_LIMIT,
        engine_api::ExecutionPayloadHeader,
    };

    use super::*;

    fn temp_db() -> Db {
        let tmp = tempdir().expect("create tempdir");
        let db_path = tmp.path().join("store.redb");
        let db = Db::new(&db_path, DbMetrics::new()).expect("db");
        db.create_tables().expect("tables");
        db
    }

    fn sample_execution_payload_header() -> ExecutionPayloadHeader {
        ExecutionPayloadHeader {
            block_hash: B256::from([1u8; 32]),
            parent_hash: B256::from([2u8; 32]),
            state_root: B256::from([3u8; 32]),
            receipts_root: B256::from([4u8; 32]),
            logs_bloom: Bloom::ZERO,
            block_number: 4242,
            gas_limit: LOAD_EXECUTION_GAS_LIMIT,
            gas_used: LOAD_EXECUTION_GAS_LIMIT / 2,
            timestamp: 1_700_000_000,
            base_fee_per_gas: U256::from(1),
            blob_gas_used: 0,
            excess_blob_gas: 0,
            prev_randao: B256::from([5u8; 32]),
            fee_recipient: Address::new([6u8; 20]),
            extra_data: AlloyBytes::new(),
            transactions_root: B256::from([7u8; 32]),
            withdrawals_root: B256::from([8u8; 32]),
            requests_hash: None,
        }
    }

    fn sample_blob_metadata(height: Height, parent_blob_root: B256) -> BlobMetadata {
        let mut commitments = Vec::new();
        for i in 0..MAX_BLOBS_PER_BLOCK_ELECTRA {
            commitments.push(KzgCommitment::new([i as u8; 48]));
        }

        let blob_hashes = commitments.iter().map(|_| B256::ZERO).collect();

        BlobMetadata::new(
            height,
            parent_blob_root,
            commitments,
            blob_hashes,
            sample_execution_payload_header(),
            Some(42),
        )
    }

    fn sample_consensus_metadata(height: Height) -> ConsensusBlockMetadata {
        ConsensusBlockMetadata::new(
            height,
            Round::new(0),
            Address::new([9u8; 20]),
            1_700_000_100,
            B256::from([10u8; 32]),
            B256::from([11u8; 32]),
            LOAD_EXECUTION_GAS_LIMIT,
            LOAD_EXECUTION_GAS_LIMIT / 4,
        )
    }

    #[test]
    fn mark_blob_metadata_decided_without_entry_returns_missing_error() {
        let db = temp_db();
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

    #[test]
    fn consensus_metadata_roundtrip_is_idempotent() {
        let db = temp_db();
        let height = Height::new(5);
        let metadata = sample_consensus_metadata(height);

        db.insert_consensus_block_metadata(height, &metadata).expect("insert");

        let fetched = db.get_consensus_block_metadata(height).expect("get").expect("metadata");
        assert_eq!(fetched, metadata);

        // Idempotent: inserting the same bytes should not error or duplicate
        db.insert_consensus_block_metadata(height, &metadata).expect("idempotent insert");

        let fetched_again =
            db.get_consensus_block_metadata(height).expect("get").expect("metadata");
        assert_eq!(fetched_again, metadata);
    }

    #[test]
    fn blob_metadata_promotion_updates_latest_pointer() {
        let db = temp_db();
        let height = Height::new(3);
        let round = Round::new(1);
        let parent_root = B256::from([12u8; 32]);
        let metadata = sample_blob_metadata(height, parent_root);

        db.insert_blob_metadata_undecided(height, round, &metadata).expect("insert undecided");

        let undecided = db
            .get_blob_metadata_undecided(height, round)
            .expect("get undecided")
            .expect("metadata present");
        assert_eq!(undecided, metadata);

        db.mark_blob_metadata_decided(height, round).expect("promote");

        assert!(
            db.get_blob_metadata_undecided(height, round).expect("get undecided").is_none(),
            "undecided entry should be removed after promotion"
        );

        let decided = db.get_blob_metadata(height).expect("get decided").expect("metadata present");
        assert_eq!(decided, metadata);

        let latest = db.get_latest_blob_metadata().expect("get latest").expect("latest metadata");
        assert_eq!(latest.0, height);
        assert_eq!(latest.1, metadata);
    }

    #[test]
    fn cleanup_gathers_only_entries_below_height() {
        let db = temp_db();
        let header = sample_execution_payload_header();

        for (h, r) in &[(1u64, 0u32), (2, 1), (3, 2)] {
            let height = Height::new(*h);
            let round = Round::new(*r);
            let parent_root =
                if *h == 1 { B256::ZERO } else { B256::from([h.to_le_bytes()[0]; 32]) };
            let metadata = if *h % 2 == 0 {
                BlobMetadata::blobless(height, parent_root, &header, Some(1))
            } else {
                let blob_hashes = vec![B256::ZERO];
                BlobMetadata::new(
                    height,
                    parent_root,
                    vec![KzgCommitment::new([*h as u8; 48])],
                    blob_hashes,
                    header.clone(),
                    Some(1),
                )
            };
            db.insert_blob_metadata_undecided(height, round, &metadata).expect("insert undecided");
        }

        let entries =
            db.get_all_undecided_blob_metadata_before(Height::new(3)).expect("get entries");

        assert_eq!(entries, vec![(Height::new(1), Round::new(0)), (Height::new(2), Round::new(1))]);
    }

    #[test]
    fn blob_metadata_undecided_roundtrip() {
        let db = temp_db();
        let height = Height::new(7);
        let round = Round::new(3);
        let metadata = sample_blob_metadata(height, B256::from([13u8; 32]));

        db.insert_blob_metadata_undecided(height, round, &metadata).expect("insert");

        let fetched =
            db.get_blob_metadata_undecided(height, round).expect("get").expect("metadata");
        assert_eq!(fetched, metadata);
    }

    #[test]
    fn blob_metadata_multi_round_isolation() {
        let db = temp_db();
        let height = Height::new(9);

        for (idx, round) in [0u32, 1, 5].into_iter().enumerate() {
            let metadata = sample_blob_metadata(height, B256::from([(20 + idx as u8); 32]));
            db.insert_blob_metadata_undecided(height, Round::new(round), &metadata)
                .expect("insert");
        }

        for (idx, round) in [0u32, 1, 5].into_iter().enumerate() {
            let fetched = db
                .get_blob_metadata_undecided(height, Round::new(round))
                .expect("get")
                .expect("metadata");
            assert_eq!(fetched.parent_blob_root(), B256::from([(20 + idx as u8); 32]));
        }
    }

    #[test]
    fn delete_blob_metadata_undecided_removes_entry() {
        let db = temp_db();
        let height = Height::new(4);
        let round = Round::new(2);
        let metadata = sample_blob_metadata(height, B256::from([21u8; 32]));

        db.insert_blob_metadata_undecided(height, round, &metadata).expect("insert");

        db.delete_blob_metadata_undecided(height, round).expect("delete");

        assert!(
            db.get_blob_metadata_undecided(height, round).expect("get").is_none(),
            "metadata should be removed after delete"
        );
    }

    #[test]
    fn latest_blob_metadata_pointer_tracks_highest_height() {
        let db = temp_db();
        let rounds = [Round::new(0), Round::new(1)];

        for height_idx in 1..=128u64 {
            let height = Height::new(height_idx);
            let metadata = sample_blob_metadata(height, B256::from([height_idx as u8; 32]));
            let round = rounds[(height_idx as usize) % rounds.len()];
            db.insert_blob_metadata_undecided(height, round, &metadata).expect("insert");
            db.mark_blob_metadata_decided(height, round).expect("promote");
        }

        let latest = db.get_latest_blob_metadata().expect("get latest").expect("metadata");
        assert_eq!(latest.0, Height::new(128));
        assert_eq!(latest.1.parent_blob_root(), B256::from([128u8; 32]));
    }

    #[test]
    fn blob_metadata_writes_are_idempotent() {
        let db = temp_db();
        let height = Height::new(11);
        let round = Round::new(4);

        let metadata = sample_blob_metadata(height, B256::from([33u8; 32]));
        db.insert_blob_metadata_undecided(height, round, &metadata).expect("insert");
        db.insert_blob_metadata_undecided(height, round, &metadata).expect("idempotent insert");

        let fetched =
            db.get_blob_metadata_undecided(height, round).expect("get").expect("metadata");
        assert_eq!(fetched, metadata);
    }
}
