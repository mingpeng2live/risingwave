// Copyright 2024 RisingWave Labs
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

mod catalog;
mod cluster;
mod error;
mod migration_plan;
mod notification;
mod stream;
mod user;

use std::collections::btree_map::{Entry, VacantEntry};
use std::collections::BTreeMap;
use std::fmt::Debug;
use std::marker::PhantomData;
use std::io::{Read, Write};
use std::ops::{Deref, DerefMut};

use anyhow::Context as _;
use async_trait::async_trait;
pub use cluster::*;
pub use error::*;
pub use migration_plan::*;
pub use notification::*;
pub use stream::*;

use crate::hummock::model::ext::Transaction as TransactionV2;
use crate::storage::{MetaStore, MetaStoreError, Snapshot, Transaction};

/// A global, unique identifier of an actor
pub type ActorId = u32;

/// Should be used together with `ActorId` to uniquely identify a dispatcher
pub type DispatcherId = u64;

/// A global, unique identifier of a fragment
pub type FragmentId = u32;

#[async_trait]
pub trait Transactional<TXN> {
    async fn upsert_in_transaction(&self, trx: &mut TXN) -> MetadataModelResult<()>;
    async fn delete_in_transaction(&self, trx: &mut TXN) -> MetadataModelResult<()>;
}

mod private {
    /// A marker trait helps to collect all implementors of `MetadataModel` in
    /// `for_all_metadata_models`. The trait should only be implemented by adding item in
    /// `for_all_metadata_models`.
    pub trait MetadataModelMarker {}
}

/// Compress the value if it's larger then the threshold to avoid hitting the limit of etcd.
///
/// By default, the maximum size of any request to etcd is 1.5 MB. So we use a slightly
/// smaller value here. However, note that this is still a best-effort approach, as the
/// compressed size may still exceed the limit, in which case we should set the parameter
/// `--max-request-bytes` of etcd to a larger value.
const MODEL_COMPRESSION_THRESHOLD: usize = 1 << 20;

/// `MetadataModel` defines basic model operations in CRUD.
// TODO: better to move the methods that we don't want implementors to override to a separate
// extension trait.
#[async_trait]
pub trait MetadataModel: std::fmt::Debug + Sized + private::MetadataModelMarker {
    /// Serialized prost message type.
    type PbType: prost::Message + Default;
    /// Serialized key type.
    type KeyType: prost::Message;

    /// Column family for this model.
    fn cf_name() -> String;

    /// Serialize to protobuf.
    fn to_protobuf(&self) -> Self::PbType;

    /// Deserialize from protobuf.
    fn from_protobuf(prost: Self::PbType) -> Self;

    /// Current record key.
    fn key(&self) -> MetadataModelResult<Self::KeyType>;

    /// Encode key to bytes. Should not be overridden.
    fn encode_key(key: &Self::KeyType) -> Vec<u8> {
        use prost::Message;
        key.encode_to_vec()
    }

    /// Encode value to bytes. Should not be overridden.
    fn encode_value(value: &Self::PbType) -> Vec<u8> {
        use flate2::write::GzEncoder;
        use flate2::Compression;
        use prost::Message;

        let pb_encoded = value.encode_to_vec();

        // Compress the value if it's larger then the threshold to avoid hitting the limit of etcd.
        if pb_encoded.len() > MODEL_COMPRESSION_THRESHOLD {
            let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
            encoder.write_all(&pb_encoded).unwrap();
            encoder.finish().unwrap()
        } else {
            pb_encoded
        }
    }

    /// Decode value from bytes. Should not be overridden.
    fn decode_value(value: &[u8]) -> MetadataModelResult<Self::PbType> {
        use flate2::bufread::GzDecoder;
        use prost::Message;

        let mut decoder = GzDecoder::new(value);
        let mut buf = Vec::new();

        // If the value is compressed, decode it.
        // This works because a protobuf-encoded message is never a valid gzip stream.
        // https://stackoverflow.com/questions/63621784/can-a-protobuf-message-begin-with-a-gzip-magic-number
        let value = if decoder.header().is_some() {
            decoder
                .read_to_end(&mut buf)
                .context("failed to decode gzipped value")?;
            buf.as_slice()
        } else {
            value
        };

        Self::PbType::decode(value).map_err(Into::into)
    }

    /// `list` returns all records in this model.
    async fn list<S>(store: &S) -> MetadataModelResult<Vec<Self>>
    where
        S: MetaStore,
    {
        let bytes_vec = store.list_cf(&Self::cf_name()).await?;
        bytes_vec
            .iter()
            .map(|(_k, v)| Self::decode_value(v.as_slice()).map(Self::from_protobuf))
            .collect()
    }

    async fn list_at_snapshot<S>(snapshot: &S::Snapshot) -> MetadataModelResult<Vec<Self>>
    where
        S: MetaStore,
    {
        let bytes_vec = snapshot.list_cf(&Self::cf_name()).await?;
        bytes_vec
            .iter()
            .map(|(_k, v)| Self::decode_value(v.as_slice()).map(Self::from_protobuf))
            .collect()
    }

    /// `insert` insert a new record in meta store, replaced it if the record already exist.
    async fn insert<S>(&self, store: &S) -> MetadataModelResult<()>
    where
        S: MetaStore,
    {
        store
            .put_cf(
                &Self::cf_name(),
                Self::encode_key(&self.key()?),
                Self::encode_value(&self.to_protobuf()),
            )
            .await
            .map_err(Into::into)
    }

    /// `delete` drop records from meta store with associated key.
    async fn delete<S>(store: &S, key: &Self::KeyType) -> MetadataModelResult<()>
    where
        S: MetaStore,
    {
        store
            .delete_cf(&Self::cf_name(), &Self::encode_key(key))
            .await
            .map_err(Into::into)
    }

    /// `select` query a record with associated key and version.
    async fn select<S>(store: &S, key: &Self::KeyType) -> MetadataModelResult<Option<Self>>
    where
        S: MetaStore,
    {
        let byte_vec = match store.get_cf(&Self::cf_name(), &Self::encode_key(key)).await {
            Ok(byte_vec) => byte_vec,
            Err(err) => {
                if !matches!(err, MetaStoreError::ItemNotFound(_)) {
                    return Err(err.into());
                }
                return Ok(None);
            }
        };
        let model = Self::from_protobuf(Self::decode_value(byte_vec.as_slice())?);
        Ok(Some(model))
    }
}

macro_rules! for_all_metadata_models {
    ($macro:ident) => {
        $macro! {
            // These items should be included in a meta snapshot.
            // So be sure to update meta backup/restore when adding new items.
            { risingwave_pb::hummock::HummockVersionStats },
            { crate::hummock::model::CompactionGroup },
            { risingwave_pb::catalog::Database },
            { risingwave_pb::catalog::Schema },
            { risingwave_pb::catalog::Table },
            { risingwave_pb::catalog::Index },
            { risingwave_pb::catalog::Sink },
            { risingwave_pb::catalog::Subscription },
            { risingwave_pb::catalog::Source },
            { risingwave_pb::catalog::View },
            { crate::model::stream::TableFragments },
            { risingwave_pb::user::UserInfo },
            { risingwave_pb::catalog::Function },
            { risingwave_pb::catalog::Connection },
            // These items need not be included in a meta snapshot.
            { crate::model::cluster::Worker },
            { risingwave_pb::hummock::CompactTaskAssignment },
            { crate::hummock::compaction::CompactStatus },
            { risingwave_hummock_sdk::version::HummockVersionDelta },
            { risingwave_pb::hummock::HummockPinnedSnapshot },
            { risingwave_pb::hummock::HummockPinnedVersion },
        }
    };
}

macro_rules! impl_metadata_model_marker {
    ($({ $target_type:ty },)*) => {
        $(
            impl private::MetadataModelMarker for $target_type {}
        )*
    }
}

for_all_metadata_models!(impl_metadata_model_marker);

/// `Transactional` defines operations supported in a transaction.
/// Read operations can be supported if necessary.
#[async_trait]
impl<T> Transactional<Transaction> for T
where
    T: MetadataModel + Sync,
{
    async fn upsert_in_transaction(&self, trx: &mut Transaction) -> MetadataModelResult<()> {
        trx.put(
            Self::cf_name(),
            Self::encode_key(&self.key()?),
            Self::encode_value(&self.to_protobuf()),
        );
        Ok(())
    }

    async fn delete_in_transaction(&self, trx: &mut Transaction) -> MetadataModelResult<()> {
        trx.delete(Self::cf_name(), Self::encode_key(&self.key()?));
        Ok(())
    }
}

/// Trait that wraps a local memory value and applies the change to the local memory value on
/// `commit` or leaves the local memory value untouched on `abort`.
pub trait ValTransaction: Sized {
    type TXN;
    /// Commit the change to local memory value
    fn commit(self);

    /// Apply the change (upsert or delete) to `txn`
    async fn apply_to_txn(&self, txn: &mut Self::TXN) -> MetadataModelResult<()>;

    /// Abort the `VarTransaction` and leave the local memory value untouched
    fn abort(self) {
        drop(self);
    }
}

/// Transaction wrapper for a variable.
/// In first `deref_mut` call, a copy of the original value will be assigned to `new_value`
/// and all subsequent modifications will be applied to the `new_value`.
/// When `commit` is called, the change to `new_value` will be applied to the `orig_value_ref`
/// When `abort` is called, the `VarTransaction` is dropped and the local memory value is
/// untouched.
pub struct VarTransaction<'a, TXN, T: Transactional<TXN>> {
    orig_value_ref: &'a mut T,
    new_value: Option<T>,
    _phantom: PhantomData<TXN>,
}

impl<'a, TXN, T> VarTransaction<'a, TXN, T>
where
    T: Transactional<TXN>,
{
    /// Create a `VarTransaction` that wraps a raw variable
    pub fn new(val_ref: &'a mut T) -> VarTransaction<'a, TXN, T> {
        VarTransaction {
            // lazy initialization
            new_value: None,
            orig_value_ref: val_ref,
            _phantom: PhantomData,
        }
    }
}

impl<'a, TXN, T: Transactional<TXN>> Deref for VarTransaction<'a, TXN, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        match &self.new_value {
            Some(new_value) => new_value,
            None => self.orig_value_ref,
        }
    }
}

impl<'a, TXN, T> DerefMut for VarTransaction<'a, TXN, T>
where
    T: Clone + Transactional<TXN>,
{
    fn deref_mut(&mut self) -> &mut Self::Target {
        if self.new_value.is_none() {
            self.new_value.replace(self.orig_value_ref.clone());
        }
        self.new_value.as_mut().unwrap()
    }
}

impl<'a, TXN, T> ValTransaction for VarTransaction<'a, TXN, T>
where
    T: Transactional<TXN> + PartialEq,
{
    type TXN = TXN;

    fn commit(self) {
        if let Some(new_value) = self.new_value {
            *self.orig_value_ref = new_value;
        }
    }

    async fn apply_to_txn(&self, txn: &mut Self::TXN) -> MetadataModelResult<()> {
        if let Some(new_value) = &self.new_value {
            // Apply the change to `txn` only when the value is modified
            if *self.orig_value_ref != *new_value {
                new_value.upsert_in_transaction(txn).await
            } else {
                Ok(())
            }
        } else {
            Ok(())
        }
    }
}

/// Represent the entry of the `staging` field of a `BTreeMapTransaction`
enum BTreeMapTransactionStagingEntry<'a, K: Ord, V> {
    /// The entry of a key does not exist in the `staging` field yet.
    Vacant(VacantEntry<'a, K, BTreeMapOp<V>>),
    /// The entry of a key exists in the `staging` field. A mutable reference to the value of the
    /// staging entry is provided for mutable access.
    Occupied(&'a mut V),
}

/// A mutable guard to the value of the corresponding key of a `BTreeMapTransaction`.
/// The staging value is initialized in a lazy manner, that is, the staging value is only cloned
/// from the original value only when it's being mutably deref.
pub struct BTreeMapTransactionValueGuard<'a, K: Ord, V: Clone> {
    // `staging_entry` is always `Some` so it's always safe to unwrap it. We make it `Option` so
    // that we can take a `Vacant` out, take its ownership, insert value into `VacantEntry` and
    // insert an `Occupied` back to the `Option`.
    // If `staging_entry` is `Vacant`, `orig_value` must be Some
    staging_entry: Option<BTreeMapTransactionStagingEntry<'a, K, V>>,
    // If the `orig_value` is None, the `staging_entry` must be `Occupied`
    orig_value: Option<&'a V>,
}

impl<'a, K: Ord, V: Clone> BTreeMapTransactionValueGuard<'a, K, V> {
    fn new(
        staging_entry: BTreeMapTransactionStagingEntry<'a, K, V>,
        orig_value: Option<&'a V>,
    ) -> Self {
        let is_entry_occupied =
            matches!(staging_entry, BTreeMapTransactionStagingEntry::Occupied(_));
        assert!(
            is_entry_occupied || orig_value.is_some(),
            "one of staging_entry and orig_value must be non-empty"
        );
        Self {
            staging_entry: Some(staging_entry),
            orig_value,
        }
    }
}

impl<'a, K: Ord, V: Clone> Deref for BTreeMapTransactionValueGuard<'a, K, V> {
    type Target = V;

    fn deref(&self) -> &Self::Target {
        // Read the staging entry first. If the staging entry is vacant, read the original value
        match &self.staging_entry.as_ref().unwrap() {
            BTreeMapTransactionStagingEntry::Vacant(_) => self
                .orig_value
                .expect("staging is vacant, so orig_value must be some"),
            BTreeMapTransactionStagingEntry::Occupied(v) => v,
        }
    }
}

impl<'a, K: Ord, V: Clone> DerefMut for BTreeMapTransactionValueGuard<'a, K, V> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        let is_occupied = matches!(
            self.staging_entry.as_ref().unwrap(),
            BTreeMapTransactionStagingEntry::Occupied(_)
        );

        // When the staging entry is vacant, take a copy of the original value and insert an entry
        // into the staging.
        if !is_occupied {
            let vacant_entry = match self.staging_entry.take().unwrap() {
                BTreeMapTransactionStagingEntry::Vacant(entry) => entry,
                BTreeMapTransactionStagingEntry::Occupied(_) => {
                    unreachable!("we have previously check that the entry is not occupied")
                }
            };

            // Insert a cloned original value to staging through `vacant_entry`
            let new_value_mut_ref = match vacant_entry.insert(BTreeMapOp::Insert(
                self.orig_value
                    .expect("self.staging_entry was vacant, so orig_value must be some")
                    .clone(),
            )) {
                BTreeMapOp::Insert(v) => v,
                BTreeMapOp::Delete => {
                    unreachable!(
                        "the previous inserted op is `Inserted`, so it's not possible to reach Delete"
                    )
                }
            };
            // Set the staging entry to `Occupied`.
            let _ = self
                .staging_entry
                .insert(BTreeMapTransactionStagingEntry::Occupied(new_value_mut_ref));
        }

        match self.staging_entry.as_mut().unwrap() {
            BTreeMapTransactionStagingEntry::Vacant(_) => {
                unreachable!("we have inserted a cloned original value in case of vacant")
            }
            BTreeMapTransactionStagingEntry::Occupied(v) => v,
        }
    }
}

enum BTreeMapOp<V> {
    Insert(V),
    Delete,
}

/// A `ValTransaction` that wraps a `BTreeMap`. It supports basic `BTreeMap` operations like `get`,
/// `get_mut`, `insert` and `remove`. Incremental modification of `insert`, `remove` and `get_mut`
/// are stored in `staging`. On `commit`, it will apply the changes stored in `staging` to the in
/// memory btree map. When serve `get` and `get_mut`, it merges the value stored in `staging` and
/// `tree_ref`.
pub struct BTreeMapTransaction<'a, K: Ord, V, TXN = Transaction> {
    /// A reference to the original `BTreeMap`. All access to this field should be immutable,
    /// except when we commit the staging changes to the original map.
    tree_ref: &'a mut BTreeMap<K, V>,
    /// Store all the staging changes that will be applied to the original map on commit
    staging: BTreeMap<K, BTreeMapOp<V>>,
    _phantom: PhantomData<TXN>,
}

impl<'a, K: Ord + Debug, V: Clone, TXN> BTreeMapTransaction<'a, K, V, TXN> {
    pub fn new(tree_ref: &'a mut BTreeMap<K, V>) -> BTreeMapTransaction<'a, K, V, TXN> {
        Self {
            tree_ref,
            staging: BTreeMap::default(),
            _phantom: PhantomData,
        }
    }

    /// Start a `BTreeMapEntryTransaction` when the `key` exists
    #[allow(dead_code)]
    pub fn new_entry_txn(&mut self, key: K) -> Option<BTreeMapEntryTransaction<'_, K, V, TXN>> {
        BTreeMapEntryTransaction::new(self.tree_ref, key, None)
    }

    /// Start a `BTreeMapEntryTransaction`. If the `key` does not exist, the the `default_val` will
    /// be taken as the initial value of the transaction and will be applied to the original
    /// `BTreeMap` on commit.
    pub fn new_entry_txn_or_default(
        &mut self,
        key: K,
        default_val: V,
    ) -> BTreeMapEntryTransaction<'_, K, V, TXN> {
        BTreeMapEntryTransaction::new(self.tree_ref, key, Some(default_val))
            .expect("default value is provided and should return `Some`")
    }

    /// Start a `BTreeMapEntryTransaction` that inserts the `val` into `key`.
    pub fn new_entry_insert_txn(
        &mut self,
        key: K,
        val: V,
    ) -> BTreeMapEntryTransaction<'_, K, V, TXN> {
        BTreeMapEntryTransaction::new_insert(self.tree_ref, key, val)
    }

    pub fn tree_ref(&self) -> &BTreeMap<K, V> {
        self.tree_ref
    }

    pub fn tree_mut(&mut self) -> &mut BTreeMap<K, V> {
        self.tree_ref
    }

    /// Get the value of the provided key by merging the staging value and the original value
    pub fn get(&self, key: &K) -> Option<&V> {
        self.staging
            .get(key)
            .and_then(|op| match op {
                BTreeMapOp::Insert(v) => Some(v),
                BTreeMapOp::Delete => None,
            })
            .or_else(|| self.tree_ref.get(key))
    }

    pub fn contains_key(&self, key: &K) -> bool {
        self.get(key).is_some()
    }

    /// This method serves the same semantic to the `get_mut` of `BTreeMap`.
    ///
    /// It return a `BTreeMapTransactionValueGuard` of the corresponding key for mutable access to
    /// guarded staging value.
    ///
    /// When the value does not exist in the staging (either key not exist or with a Delete record)
    /// and the value does not exist in the original `BTreeMap`, return None.
    pub fn get_mut(&mut self, key: K) -> Option<BTreeMapTransactionValueGuard<'_, K, V>> {
        let orig_contains_key = self.tree_ref.contains_key(&key);
        let orig_value = self.tree_ref.get(&key);

        let staging_entry = match self.staging.entry(key) {
            Entry::Occupied(entry) => match entry.into_mut() {
                BTreeMapOp::Insert(v) => BTreeMapTransactionStagingEntry::Occupied(v),
                BTreeMapOp::Delete => return None,
            },
            Entry::Vacant(vacant_entry) => {
                if !orig_contains_key {
                    return None;
                } else {
                    BTreeMapTransactionStagingEntry::Vacant(vacant_entry)
                }
            }
        };
        Some(BTreeMapTransactionValueGuard::new(
            staging_entry,
            orig_value,
        ))
    }

    pub fn insert(&mut self, key: K, value: V) {
        self.staging.insert(key, BTreeMapOp::Insert(value));
    }

    pub fn remove(&mut self, key: K) -> Option<V> {
        if let Some(op) = self.staging.get(&key) {
            return match op {
                BTreeMapOp::Delete => None,
                BTreeMapOp::Insert(_) => match self.staging.remove(&key).unwrap() {
                    BTreeMapOp::Insert(v) => {
                        self.staging.insert(key, BTreeMapOp::Delete);
                        Some(v)
                    }
                    BTreeMapOp::Delete => {
                        unreachable!("we have checked that the op of the key is `Insert`, so it's impossible to be Delete")
                    }
                },
            };
        }
        match self.tree_ref.get(&key) {
            Some(orig_value) => {
                self.staging.insert(key, BTreeMapOp::Delete);
                Some(orig_value.clone())
            }
            None => None,
        }
    }

    pub fn commit_memory(self) {
        // Apply each op stored in the staging to original tree.
        for (k, op) in self.staging {
            match op {
                BTreeMapOp::Insert(v) => {
                    self.tree_ref.insert(k, v);
                }
                BTreeMapOp::Delete => {
                    self.tree_ref.remove(&k);
                }
            }
        }
    }
}

impl<'a, K: Ord + Debug, V: Transactional<TXN> + Clone, TXN> ValTransaction
    for BTreeMapTransaction<'a, K, V, TXN>
{
    type TXN = TXN;

    fn commit(self) {
        self.commit_memory();
    }

    async fn apply_to_txn(&self, txn: &mut Self::TXN) -> MetadataModelResult<()> {
        // Add the staging operation to txn
        for (k, op) in &self.staging {
            match op {
                BTreeMapOp::Insert(v) => v.upsert_in_transaction(txn).await?,
                BTreeMapOp::Delete => {
                    if let Some(v) = self.tree_ref.get(k) {
                        v.delete_in_transaction(txn).await?;
                    }
                }
            }
        }
        Ok(())
    }
}

/// Transaction wrapper for a `BTreeMap` entry value of given `key`
pub struct BTreeMapEntryTransaction<'a, K, V, TXN> {
    tree_ref: &'a mut BTreeMap<K, V>,
    pub key: K,
    pub new_value: V,
    _phantom: PhantomData<TXN>,
}

impl<'a, K: Ord + Debug, V: Clone, TXN> BTreeMapEntryTransaction<'a, K, V, TXN> {
    /// Create a `ValTransaction` that wraps a `BTreeMap` entry of the given `key`.
    /// If the tree does not contain `key`, the `default_val` will be used as the initial value
    pub fn new_insert(
        tree_ref: &'a mut BTreeMap<K, V>,
        key: K,
        value: V,
    ) -> BTreeMapEntryTransaction<'a, K, V, TXN> {
        BTreeMapEntryTransaction {
            new_value: value,
            tree_ref,
            key,
            _phantom: PhantomData,
        }
    }

    /// Create a `BTreeMapEntryTransaction` that wraps a `BTreeMap` entry of the given `key`.
    /// If the `key` exists in the tree, return `Some` of a `BTreeMapEntryTransaction` wrapped for
    /// the of the given `key`.
    /// If the `key` does not exist in the tree but `default_val` is provided as `Some`, a
    /// `BTreeMapEntryTransaction` that wraps the given `key` and default value is returned
    /// Otherwise return `None`.
    pub fn new(
        tree_ref: &'a mut BTreeMap<K, V>,
        key: K,
        default_val: Option<V>,
    ) -> Option<BTreeMapEntryTransaction<'a, K, V, TXN>> {
        tree_ref
            .get(&key)
            .cloned()
            .or(default_val)
            .map(|orig_value| BTreeMapEntryTransaction {
                new_value: orig_value,
                tree_ref,
                key,
                _phantom: PhantomData,
            })
    }
}

impl<'a, K, V, TXN> Deref for BTreeMapEntryTransaction<'a, K, V, TXN> {
    type Target = V;

    fn deref(&self) -> &Self::Target {
        &self.new_value
    }
}

impl<'a, K, V, TXN> DerefMut for BTreeMapEntryTransaction<'a, K, V, TXN> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.new_value
    }
}

impl<'a, K: Ord, V: PartialEq + Transactional<TXN>, TXN> ValTransaction
    for BTreeMapEntryTransaction<'a, K, V, TXN>
{
    type TXN = TXN;

    fn commit(self) {
        self.tree_ref.insert(self.key, self.new_value);
    }

    async fn apply_to_txn(&self, txn: &mut Self::TXN) -> MetadataModelResult<()> {
        if !self.tree_ref.contains_key(&self.key)
            || *self.tree_ref.get(&self.key).unwrap() != self.new_value
        {
            self.new_value.upsert_in_transaction(txn).await?
        }
        Ok(())
    }
}

pub enum BTreeMapTransactionWrapper<'a, K: Ord, V> {
    V1(BTreeMapTransaction<'a, K, V, Transaction>),
    V2(BTreeMapTransaction<'a, K, V, TransactionV2>),
}

impl<'a, K: Ord + Debug, V: Clone> BTreeMapTransactionWrapper<'a, K, V> {
    pub fn tree_ref(&self) -> &BTreeMap<K, V> {
        match self {
            BTreeMapTransactionWrapper::V1(v) => v.tree_ref,
            BTreeMapTransactionWrapper::V2(v) => v.tree_ref,
        }
    }

    pub fn tree_mut(&mut self) -> &mut BTreeMap<K, V> {
        match self {
            BTreeMapTransactionWrapper::V1(v) => v.tree_ref,
            BTreeMapTransactionWrapper::V2(v) => v.tree_ref,
        }
    }

    /// Get the value of the provided key by merging the staging value and the original value
    pub fn get(&self, key: &K) -> Option<&V> {
        match self {
            BTreeMapTransactionWrapper::V1(v) => v.get(key),
            BTreeMapTransactionWrapper::V2(v) => v.get(key),
        }
    }

    pub fn contains_key(&self, key: &K) -> bool {
        match self {
            BTreeMapTransactionWrapper::V1(v) => v.contains_key(key),
            BTreeMapTransactionWrapper::V2(v) => v.contains_key(key),
        }
    }

    pub fn get_mut(&mut self, key: K) -> Option<BTreeMapTransactionValueGuard<'_, K, V>> {
        match self {
            BTreeMapTransactionWrapper::V1(v) => v.get_mut(key),
            BTreeMapTransactionWrapper::V2(v) => v.get_mut(key),
        }
    }

    pub fn insert(&mut self, key: K, value: V) {
        match self {
            BTreeMapTransactionWrapper::V1(v) => v.insert(key, value),
            BTreeMapTransactionWrapper::V2(v) => v.insert(key, value),
        }
    }

    pub fn remove(&mut self, key: K) -> Option<V> {
        match self {
            BTreeMapTransactionWrapper::V1(v) => v.remove(key),
            BTreeMapTransactionWrapper::V2(v) => v.remove(key),
        }
    }

    pub fn commit_memory(self) {
        match self {
            BTreeMapTransactionWrapper::V1(v) => v.commit_memory(),
            BTreeMapTransactionWrapper::V2(v) => v.commit_memory(),
        }
    }

    pub fn new_entry_txn_or_default(
        &mut self,
        key: K,
        default_val: V,
    ) -> BTreeMapEntryTransactionWrapper<'_, K, V> {
        match self {
            BTreeMapTransactionWrapper::V1(v) => BTreeMapEntryTransactionWrapper::V1(
                BTreeMapEntryTransaction::new(v.tree_ref, key, Some(default_val))
                    .expect("default value is provided and should return `Some`"),
            ),
            BTreeMapTransactionWrapper::V2(v) => BTreeMapEntryTransactionWrapper::V2(
                BTreeMapEntryTransaction::new(v.tree_ref, key, Some(default_val))
                    .expect("default value is provided and should return `Some`"),
            ),
        }
    }

    pub fn new_entry_insert_txn(
        &mut self,
        key: K,
        val: V,
    ) -> BTreeMapEntryTransactionWrapper<'_, K, V> {
        match self {
            BTreeMapTransactionWrapper::V1(v) => BTreeMapEntryTransactionWrapper::V1(
                BTreeMapEntryTransaction::new_insert(v.tree_ref, key, val),
            ),
            BTreeMapTransactionWrapper::V2(v) => BTreeMapEntryTransactionWrapper::V2(
                BTreeMapEntryTransaction::new_insert(v.tree_ref, key, val),
            ),
        }
    }
}

impl<'a, K: Ord + Debug, V: Clone> BTreeMapTransactionWrapper<'a, K, V> {
    pub fn into_v1(self) -> BTreeMapTransaction<'a, K, V, Transaction> {
        match self {
            BTreeMapTransactionWrapper::V1(v) => v,
            BTreeMapTransactionWrapper::V2(_) => panic!("expect V1, found V2"),
        }
    }

    pub fn as_v1_ref(&self) -> &BTreeMapTransaction<'a, K, V, Transaction> {
        match self {
            BTreeMapTransactionWrapper::V1(v) => v,
            BTreeMapTransactionWrapper::V2(_) => panic!("expect V1, found V2"),
        }
    }

    pub fn into_v2(self) -> BTreeMapTransaction<'a, K, V, TransactionV2> {
        match self {
            BTreeMapTransactionWrapper::V1(_) => panic!("expect V2, found V1"),
            BTreeMapTransactionWrapper::V2(v) => v,
        }
    }

    pub fn as_v2_ref(&self) -> &BTreeMapTransaction<'a, K, V, TransactionV2> {
        match self {
            BTreeMapTransactionWrapper::V1(_) => panic!("expect V2, found V1"),
            BTreeMapTransactionWrapper::V2(v) => v,
        }
    }
}

pub enum BTreeMapEntryTransactionWrapper<'a, K, V> {
    V1(BTreeMapEntryTransaction<'a, K, V, Transaction>),
    V2(BTreeMapEntryTransaction<'a, K, V, TransactionV2>),
}

impl<'a, K: Ord + Debug, V: Clone> Deref for BTreeMapEntryTransactionWrapper<'a, K, V> {
    type Target = V;

    fn deref(&self) -> &Self::Target {
        match self {
            BTreeMapEntryTransactionWrapper::V1(v) => v.deref(),
            BTreeMapEntryTransactionWrapper::V2(v) => v.deref(),
        }
    }
}

impl<'a, K: Ord + Debug, V: Clone> DerefMut for BTreeMapEntryTransactionWrapper<'a, K, V> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        match self {
            BTreeMapEntryTransactionWrapper::V1(v) => v.deref_mut(),
            BTreeMapEntryTransactionWrapper::V2(v) => v.deref_mut(),
        }
    }
}

impl<'a, K: Ord + Debug, V: Clone> BTreeMapEntryTransactionWrapper<'a, K, V> {
    pub fn as_v1_ref(&self) -> &BTreeMapEntryTransaction<'a, K, V, Transaction> {
        match self {
            BTreeMapEntryTransactionWrapper::V1(v) => v,
            BTreeMapEntryTransactionWrapper::V2(_) => {
                panic!("expect V1, found V2")
            }
        }
    }

    pub fn into_v1(self) -> BTreeMapEntryTransaction<'a, K, V, Transaction> {
        match self {
            BTreeMapEntryTransactionWrapper::V1(v) => v,
            BTreeMapEntryTransactionWrapper::V2(_) => {
                panic!("expect V1, found V2")
            }
        }
    }

    pub fn as_v2_ref(&self) -> &BTreeMapEntryTransaction<'a, K, V, TransactionV2> {
        match self {
            BTreeMapEntryTransactionWrapper::V1(_) => {
                panic!("expect V2, found V1")
            }
            BTreeMapEntryTransactionWrapper::V2(v) => v,
        }
    }

    pub fn into_v2(self) -> BTreeMapEntryTransaction<'a, K, V, TransactionV2> {
        match self {
            BTreeMapEntryTransactionWrapper::V1(_) => {
                panic!("expect V2, found V1")
            }
            BTreeMapEntryTransactionWrapper::V2(v) => v,
        }
    }
}

pub enum VarTransactionWrapper<'a, T: Transactional<Transaction> + Transactional<TransactionV2>> {
    V1(VarTransaction<'a, Transaction, T>),
    V2(VarTransaction<'a, TransactionV2, T>),
}

impl<'a, T: Transactional<Transaction> + Transactional<TransactionV2>>
    VarTransactionWrapper<'a, T>
{
    pub fn as_v1_ref(&self) -> &VarTransaction<'a, Transaction, T> {
        match self {
            VarTransactionWrapper::V1(v) => v,
            VarTransactionWrapper::V2(_) => {
                panic!("expect V1, found V2")
            }
        }
    }

    pub fn into_v1(self) -> VarTransaction<'a, Transaction, T> {
        match self {
            VarTransactionWrapper::V1(v) => v,
            VarTransactionWrapper::V2(_) => {
                panic!("expect V1, found V2")
            }
        }
    }

    pub fn as_v2_ref(&self) -> &VarTransaction<'a, TransactionV2, T> {
        match self {
            VarTransactionWrapper::V1(_) => {
                panic!("expect V2, found V1")
            }
            VarTransactionWrapper::V2(v) => v,
        }
    }

    pub fn into_v2(self) -> VarTransaction<'a, TransactionV2, T> {
        match self {
            VarTransactionWrapper::V1(_) => {
                panic!("expect V2, found V1")
            }
            VarTransactionWrapper::V2(v) => v,
        }
    }
}

impl<'a, T: Transactional<Transaction> + Transactional<TransactionV2>> Deref
    for VarTransactionWrapper<'a, T>
{
    type Target = T;

    fn deref(&self) -> &Self::Target {
        match self {
            VarTransactionWrapper::V1(v) => v.deref(),
            VarTransactionWrapper::V2(v) => v.deref(),
        }
    }
}

impl<'a, T: Transactional<Transaction> + Transactional<TransactionV2> + Clone> DerefMut
    for VarTransactionWrapper<'a, T>
{
    fn deref_mut(&mut self) -> &mut Self::Target {
        match self {
            VarTransactionWrapper::V1(v) => v.deref_mut(),
            VarTransactionWrapper::V2(v) => v.deref_mut(),
        }
    }
}

#[cfg(test)]
mod tests {
    use itertools::Itertools;

    use super::*;
    use crate::storage::Operation;

    #[derive(PartialEq, Clone, Debug)]
    struct TestTransactional {
        key: &'static str,
        value: &'static str,
    }

    const TEST_CF: &str = "test-cf";

    #[async_trait]
    impl Transactional<Transaction> for TestTransactional {
        async fn upsert_in_transaction(&self, trx: &mut Transaction) -> MetadataModelResult<()> {
            trx.put(
                TEST_CF.to_string(),
                self.key.as_bytes().into(),
                self.value.as_bytes().into(),
            );
            Ok(())
        }

        async fn delete_in_transaction(&self, trx: &mut Transaction) -> MetadataModelResult<()> {
            trx.delete(TEST_CF.to_string(), self.key.as_bytes().into());
            Ok(())
        }
    }

    #[tokio::test]
    async fn test_compress_decompress() {
        use prost::Message;
        use risingwave_pb::catalog::Database;

        use crate::storage::MemStore;

        async fn do_test(len: usize) {
            // Use `Database` as a test model.
            type Model = Database;

            let store = MemStore::new();
            let model = Model {
                name: "t".repeat(len),
                ..Default::default()
            };
            {
                let encoded_len = model.encoded_len();
                // Showing that the encoded length is larger than the original length.
                // So that a len greater than the threshold will hit the compression branch.
                assert!(encoded_len >= len, "encoded_len: {encoded_len}, len: {len}");
            }
            model.insert(&store).await.unwrap();

            // Test `list`
            let decoded = Model::list(&store)
                .await
                .unwrap()
                .into_iter()
                .exactly_one()
                .unwrap();
            assert_eq!(model, decoded);

            // Test `select`
            let decoded = Model::select(&store, &model.key().unwrap())
                .await
                .unwrap()
                .into_iter()
                .exactly_one()
                .unwrap();
            assert_eq!(model, decoded);
        }

        do_test(1).await;
        do_test(MODEL_COMPRESSION_THRESHOLD + 1).await;
    }

    #[tokio::test]
    async fn test_simple_var_transaction_commit() {
        let mut kv = TestTransactional {
            key: "key",
            value: "original",
        };
        let mut num_txn = VarTransaction::new(&mut kv);
        num_txn.value = "modified";
        assert_eq!(num_txn.value, "modified");
        let mut txn = Transaction::default();
        num_txn.apply_to_txn(&mut txn).await.unwrap();
        let txn_op = txn.get_operations();
        assert_eq!(1, txn_op.len());
        assert!(matches!(
            &txn_op[0],
            Operation::Put {
                cf: _,
                key: _,
                value: _
            }
        ));
        assert!(
            matches!(&txn_op[0], Operation::Put { cf, key, value } if *cf == TEST_CF && key == "key".as_bytes() && value == "modified".as_bytes())
        );
        num_txn.commit();
        assert_eq!("modified", kv.value);
    }

    #[test]
    fn test_simple_var_transaction_abort() {
        let mut kv = TestTransactional {
            key: "key",
            value: "original",
        };
        let mut num_txn = VarTransaction::new(&mut kv);
        num_txn.value = "modified";
        num_txn.abort();
        assert_eq!("original", kv.value);
    }

    #[tokio::test]
    async fn test_tree_map_transaction_commit() {
        let mut map: BTreeMap<String, TestTransactional> = BTreeMap::new();
        map.insert(
            "to-remove".to_string(),
            TestTransactional {
                key: "to-remove",
                value: "to-remove-value",
            },
        );
        map.insert(
            "to-remove-after-modify".to_string(),
            TestTransactional {
                key: "to-remove-after-modify",
                value: "to-remove-after-modify-value",
            },
        );
        map.insert(
            "first".to_string(),
            TestTransactional {
                key: "first",
                value: "first-orig-value",
            },
        );

        let mut map_copy = map.clone();
        let mut map_txn = BTreeMapTransaction::new(&mut map);
        map_txn.remove("to-remove".to_string());
        map_txn.insert(
            "to-remove-after-modify".to_string(),
            TestTransactional {
                key: "to-remove-after-modify",
                value: "to-remove-after-modify-value-modifying",
            },
        );
        map_txn.remove("to-remove-after-modify".to_string());
        map_txn.insert(
            "first".to_string(),
            TestTransactional {
                key: "first",
                value: "first-value",
            },
        );
        map_txn.insert(
            "second".to_string(),
            TestTransactional {
                key: "second",
                value: "second-value",
            },
        );
        assert_eq!(
            &TestTransactional {
                key: "second",
                value: "second-value",
            },
            map_txn.get(&"second".to_string()).unwrap()
        );
        map_txn.insert(
            "third".to_string(),
            TestTransactional {
                key: "third",
                value: "third-value",
            },
        );
        assert_eq!(
            &TestTransactional {
                key: "third",
                value: "third-value",
            },
            map_txn.get(&"third".to_string()).unwrap()
        );

        let mut third_entry = map_txn.get_mut("third".to_string()).unwrap();
        third_entry.value = "third-value-updated";
        assert_eq!(
            &TestTransactional {
                key: "third",
                value: "third-value-updated",
            },
            map_txn.get(&"third".to_string()).unwrap()
        );

        let mut txn = Transaction::default();
        map_txn.apply_to_txn(&mut txn).await.unwrap();
        let txn_ops = txn.get_operations();
        assert_eq!(5, txn_ops.len());
        for op in txn_ops {
            match op {
                Operation::Put { cf, key, value }
                    if cf == TEST_CF
                        && key == "first".as_bytes()
                        && value == "first-value".as_bytes() => {}
                Operation::Put { cf, key, value }
                    if cf == TEST_CF
                        && key == "second".as_bytes()
                        && value == "second-value".as_bytes() => {}
                Operation::Put { cf, key, value }
                    if cf == TEST_CF
                        && key == "third".as_bytes()
                        && value == "third-value-updated".as_bytes() => {}
                Operation::Delete { cf, key } if cf == TEST_CF && key == "to-remove".as_bytes() => {
                }
                Operation::Delete { cf, key }
                    if cf == TEST_CF && key == "to-remove-after-modify".as_bytes() => {}
                _ => unreachable!("invalid operation"),
            }
        }
        map_txn.commit();

        // replay the change to local copy and compare
        map_copy.remove("to-remove").unwrap();
        map_copy.insert(
            "to-remove-after-modify".to_string(),
            TestTransactional {
                key: "to-remove-after-modify",
                value: "to-remove-after-modify-value-modifying",
            },
        );
        map_copy.remove("to-remove-after-modify").unwrap();
        map_copy.insert(
            "first".to_string(),
            TestTransactional {
                key: "first",
                value: "first-value",
            },
        );
        map_copy.insert(
            "second".to_string(),
            TestTransactional {
                key: "second",
                value: "second-value",
            },
        );
        map_copy.insert(
            "third".to_string(),
            TestTransactional {
                key: "third",
                value: "third-value-updated",
            },
        );
        assert_eq!(map_copy, map);
    }

    #[tokio::test]
    async fn test_tree_map_entry_update_transaction_commit() {
        let mut map: BTreeMap<String, TestTransactional> = BTreeMap::new();
        map.insert(
            "first".to_string(),
            TestTransactional {
                key: "first",
                value: "first-orig-value",
            },
        );

        let mut map_txn = BTreeMapTransaction::new(&mut map);
        let mut first_entry_txn = map_txn.new_entry_txn("first".to_string()).unwrap();
        first_entry_txn.value = "first-value";
        let mut txn = Transaction::default();
        first_entry_txn.apply_to_txn(&mut txn).await.unwrap();
        let txn_ops = txn.get_operations();
        assert_eq!(1, txn_ops.len());
        assert!(
            matches!(&txn_ops[0], Operation::Put {cf, key, value} if *cf == TEST_CF && key == "first".as_bytes() && value == "first-value".as_bytes())
        );
        first_entry_txn.commit();
        assert_eq!("first-value", map.get("first").unwrap().value);
    }

    #[tokio::test]
    async fn test_tree_map_entry_insert_transaction_commit() {
        let mut map: BTreeMap<String, TestTransactional> = BTreeMap::new();

        let mut map_txn = BTreeMapTransaction::new(&mut map);
        let first_entry_txn = map_txn.new_entry_insert_txn(
            "first".to_string(),
            TestTransactional {
                key: "first",
                value: "first-value",
            },
        );
        let mut txn = Transaction::default();
        first_entry_txn.apply_to_txn(&mut txn).await.unwrap();
        let txn_ops = txn.get_operations();
        assert_eq!(1, txn_ops.len());
        assert!(
            matches!(&txn_ops[0], Operation::Put {cf, key, value} if *cf == TEST_CF && key == "first".as_bytes() && value == "first-value".as_bytes())
        );
        first_entry_txn.commit();
        assert_eq!("first-value", map.get("first").unwrap().value);
    }
}
