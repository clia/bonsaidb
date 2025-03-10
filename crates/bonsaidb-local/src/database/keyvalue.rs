use std::borrow::Cow;
use std::collections::{btree_map, BTreeMap, VecDeque};
use std::sync::{Arc, Weak};
use std::time::Duration;

use bonsaidb_core::connection::{Connection, HasSession};
use bonsaidb_core::keyvalue::{
    Command, KeyCheck, KeyOperation, KeyStatus, KeyValue, Numeric, Output, SetCommand, Timestamp,
    Value,
};
use bonsaidb_core::permissions::bonsai::{
    keyvalue_key_resource_name, BonsaiAction, DatabaseAction, KeyValueAction,
};
use bonsaidb_core::transaction::{ChangedKey, Changes};
use nebari::io::any::AnyFile;
use nebari::tree::{CompareSwap, Operation, Root, ScanEvaluation, Unversioned};
use nebari::{AbortError, ArcBytes, Roots};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use watchable::{Watchable, Watcher};

use crate::config::KeyValuePersistence;
use crate::database::compat;
use crate::storage::StorageLock;
use crate::tasks::{Job, Keyed, Task};
use crate::{Database, DatabaseNonBlocking, Error};

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Entry {
    pub value: Value,
    pub expiration: Option<Timestamp>,
    #[serde(default)]
    pub last_updated: Timestamp,
}

impl Entry {
    pub(crate) fn restore(
        self,
        namespace: Option<String>,
        key: String,
        database: &Database,
    ) -> Result<(), bonsaidb_core::Error> {
        database.execute_key_operation(KeyOperation {
            namespace,
            key,
            command: Command::Set(SetCommand {
                value: self.value,
                expiration: self.expiration,
                keep_existing_expiration: false,
                check: None,
                return_previous_value: false,
            }),
        })?;
        Ok(())
    }
}

impl KeyValue for Database {
    fn execute_key_operation(&self, op: KeyOperation) -> Result<Output, bonsaidb_core::Error> {
        self.check_permission(
            keyvalue_key_resource_name(self.name(), op.namespace.as_deref(), &op.key),
            &BonsaiAction::Database(DatabaseAction::KeyValue(KeyValueAction::ExecuteOperation)),
        )?;
        self.data.context.perform_kv_operation(op)
    }
}

impl Database {
    pub(crate) fn all_key_value_entries(
        &self,
    ) -> Result<BTreeMap<(Option<String>, String), Entry>, Error> {
        // Lock the state so that new new modifications can be made while we gather this snapshot.
        let state = self.data.context.key_value_state.lock();
        let database = self.clone();
        // Initialize our entries with any dirty keys and any keys that are about to be persisted.
        let mut all_entries = BTreeMap::new();
        database
            .roots()
            .tree(Unversioned::tree(KEY_TREE))?
            .scan::<Error, _, _, _, _>(
                &(..),
                true,
                |_, _, _| ScanEvaluation::ReadData,
                |_, _| ScanEvaluation::ReadData,
                |key, _, entry: ArcBytes<'static>| {
                    let entry = bincode::deserialize::<Entry>(&entry)
                        .map_err(|err| AbortError::Other(Error::from(err)))?;
                    let full_key = std::str::from_utf8(&key)
                        .map_err(|err| AbortError::Other(Error::from(err)))?;

                    if let Some(split_key) = split_key(full_key) {
                        // Do not overwrite the existing key
                        all_entries.entry(split_key).or_insert(entry);
                    }

                    Ok(())
                },
            )?;

        // Apply the pending writes first
        if let Some(pending_keys) = &state.keys_being_persisted {
            for (key, possible_entry) in pending_keys.iter() {
                let (namespace, key) = split_key(key).unwrap();
                if let Some(updated_entry) = possible_entry {
                    all_entries.insert((namespace, key), updated_entry.clone());
                } else {
                    all_entries.remove(&(namespace, key));
                }
            }
        }

        for (key, possible_entry) in &state.dirty_keys {
            let (namespace, key) = split_key(key).unwrap();
            if let Some(updated_entry) = possible_entry {
                all_entries.insert((namespace, key), updated_entry.clone());
            } else {
                all_entries.remove(&(namespace, key));
            }
        }

        Ok(all_entries)
    }
}

pub(crate) const KEY_TREE: &str = "kv";

fn full_key(namespace: Option<&str>, key: &str) -> String {
    let full_length = namespace.map_or_else(|| 0, str::len) + key.len() + 1;
    let mut full_key = String::with_capacity(full_length);
    if let Some(ns) = namespace {
        full_key.push_str(ns);
    }
    full_key.push('\0');
    full_key.push_str(key);
    full_key
}

fn split_key(full_key: &str) -> Option<(Option<String>, String)> {
    if let Some((namespace, key)) = full_key.split_once('\0') {
        let namespace = if namespace.is_empty() {
            None
        } else {
            Some(namespace.to_string())
        };
        Some((namespace, key.to_string()))
    } else {
        None
    }
}

fn increment(existing: &Numeric, amount: &Numeric, saturating: bool) -> Numeric {
    match amount {
        Numeric::Integer(amount) => {
            let existing_value = existing.as_i64_lossy(saturating);
            let new_value = if saturating {
                existing_value.saturating_add(*amount)
            } else {
                existing_value.wrapping_add(*amount)
            };
            Numeric::Integer(new_value)
        }
        Numeric::UnsignedInteger(amount) => {
            let existing_value = existing.as_u64_lossy(saturating);
            let new_value = if saturating {
                existing_value.saturating_add(*amount)
            } else {
                existing_value.wrapping_add(*amount)
            };
            Numeric::UnsignedInteger(new_value)
        }
        Numeric::Float(amount) => {
            let existing_value = existing.as_f64_lossy();
            let new_value = existing_value + *amount;
            Numeric::Float(new_value)
        }
    }
}

fn decrement(existing: &Numeric, amount: &Numeric, saturating: bool) -> Numeric {
    match amount {
        Numeric::Integer(amount) => {
            let existing_value = existing.as_i64_lossy(saturating);
            let new_value = if saturating {
                existing_value.saturating_sub(*amount)
            } else {
                existing_value.wrapping_sub(*amount)
            };
            Numeric::Integer(new_value)
        }
        Numeric::UnsignedInteger(amount) => {
            let existing_value = existing.as_u64_lossy(saturating);
            let new_value = if saturating {
                existing_value.saturating_sub(*amount)
            } else {
                existing_value.wrapping_sub(*amount)
            };
            Numeric::UnsignedInteger(new_value)
        }
        Numeric::Float(amount) => {
            let existing_value = existing.as_f64_lossy();
            let new_value = existing_value - *amount;
            Numeric::Float(new_value)
        }
    }
}

#[derive(Debug)]
pub struct KeyValueState {
    roots: Roots<AnyFile>,
    persistence: KeyValuePersistence,
    last_commit: Timestamp,
    background_worker_target: Watchable<BackgroundWorkerProcessTarget>,
    expiring_keys: BTreeMap<String, Timestamp>,
    expiration_order: VecDeque<String>,
    dirty_keys: BTreeMap<String, Option<Entry>>,
    keys_being_persisted: Option<Arc<BTreeMap<String, Option<Entry>>>>,
    last_persistence: Watchable<Timestamp>,
    shutdown: Option<flume::Sender<()>>,
}

impl KeyValueState {
    pub fn new(
        persistence: KeyValuePersistence,
        roots: Roots<AnyFile>,
        background_worker_target: Watchable<BackgroundWorkerProcessTarget>,
    ) -> Self {
        Self {
            roots,
            persistence,
            last_commit: Timestamp::now(),
            expiring_keys: BTreeMap::new(),
            background_worker_target,
            expiration_order: VecDeque::new(),
            dirty_keys: BTreeMap::new(),
            keys_being_persisted: None,
            last_persistence: Watchable::new(Timestamp::MIN),
            shutdown: None,
        }
    }

    pub fn shutdown(&mut self, state: &Arc<Mutex<KeyValueState>>) -> Option<flume::Receiver<()>> {
        if self.keys_being_persisted.is_none() && self.commit_dirty_keys(state) {
            let (shutdown_sender, shutdown_receiver) = flume::bounded(1);
            self.shutdown = Some(shutdown_sender);
            Some(shutdown_receiver)
        } else {
            None
        }
    }

    pub fn perform_kv_operation(
        &mut self,
        op: KeyOperation,
        state: &Arc<Mutex<KeyValueState>>,
    ) -> Result<Output, bonsaidb_core::Error> {
        let now = Timestamp::now();
        // If there are any keys that have expired, clear them before executing any operations.
        self.remove_expired_keys(now);
        let result = match op.command {
            Command::Set(command) => {
                self.execute_set_operation(op.namespace.as_deref(), &op.key, command, now)
            }
            Command::Get { delete } => {
                self.execute_get_operation(op.namespace.as_deref(), &op.key, delete)
            }
            Command::Delete => self.execute_delete_operation(op.namespace.as_deref(), &op.key),
            Command::Increment { amount, saturating } => self.execute_increment_operation(
                op.namespace.as_deref(),
                &op.key,
                &amount,
                saturating,
                now,
            ),
            Command::Decrement { amount, saturating } => self.execute_decrement_operation(
                op.namespace.as_deref(),
                &op.key,
                &amount,
                saturating,
                now,
            ),
        };
        if result.is_ok() {
            if self.needs_commit(now) {
                self.commit_dirty_keys(state);
            }
            self.update_background_worker_target();
        }
        result
    }

    #[cfg_attr(
        feature = "tracing",
        tracing::instrument(level = "trace", skip(self, set, now),)
    )]
    fn execute_set_operation(
        &mut self,
        namespace: Option<&str>,
        key: &str,
        set: SetCommand,
        now: Timestamp,
    ) -> Result<Output, bonsaidb_core::Error> {
        let mut entry = Entry {
            value: set.value.validate()?,
            expiration: set.expiration,
            last_updated: now,
        };
        let full_key = full_key(namespace, key);
        let possible_existing_value =
            if set.check.is_some() || set.return_previous_value || set.keep_existing_expiration {
                Some(self.get(&full_key).map_err(Error::from)?)
            } else {
                None
            };
        let existing_value_ref = possible_existing_value.as_ref().and_then(Option::as_ref);

        let updating = match set.check {
            Some(KeyCheck::OnlyIfPresent) => existing_value_ref.is_some(),
            Some(KeyCheck::OnlyIfVacant) => existing_value_ref.is_none(),
            None => true,
        };
        if updating {
            if set.keep_existing_expiration {
                if let Some(existing_value) = existing_value_ref {
                    entry.expiration = existing_value.expiration;
                }
            }
            self.update_key_expiration(&full_key, entry.expiration);

            let previous_value = if let Some(existing_value) = possible_existing_value {
                // we already fetched, no need to ask for the existing value back
                self.set(full_key, entry);
                existing_value
            } else {
                self.replace(full_key, entry).map_err(Error::from)?
            };
            if set.return_previous_value {
                Ok(Output::Value(previous_value.map(|entry| entry.value)))
            } else if previous_value.is_none() {
                Ok(Output::Status(KeyStatus::Inserted))
            } else {
                Ok(Output::Status(KeyStatus::Updated))
            }
        } else {
            Ok(Output::Status(KeyStatus::NotChanged))
        }
    }

    #[cfg_attr(
        feature = "tracing",
        tracing::instrument(level = "trace", skip(self, tree_key, expiration))
    )]
    pub fn update_key_expiration<'key>(
        &mut self,
        tree_key: impl Into<Cow<'key, str>>,
        expiration: Option<Timestamp>,
    ) {
        let tree_key = tree_key.into();
        let mut changed_first_expiration = false;
        if let Some(expiration) = expiration {
            let key = if self.expiring_keys.contains_key(tree_key.as_ref()) {
                // Update the existing entry.
                let existing_entry_index = self
                    .expiration_order
                    .iter()
                    .enumerate()
                    .find_map(
                        |(index, key)| {
                            if &tree_key == key {
                                Some(index)
                            } else {
                                None
                            }
                        },
                    )
                    .unwrap();
                changed_first_expiration = existing_entry_index == 0;
                self.expiration_order.remove(existing_entry_index).unwrap()
            } else {
                tree_key.into_owned()
            };

            // Insert the key into the expiration_order queue
            let mut insert_at = None;
            for (index, expiring_key) in self.expiration_order.iter().enumerate() {
                if self.expiring_keys.get(expiring_key).unwrap() > &expiration {
                    insert_at = Some(index);
                    break;
                }
            }
            if let Some(insert_at) = insert_at {
                changed_first_expiration |= insert_at == 0;

                self.expiration_order.insert(insert_at, key.clone());
            } else {
                changed_first_expiration |= self.expiration_order.is_empty();
                self.expiration_order.push_back(key.clone());
            }
            self.expiring_keys.insert(key, expiration);
        } else if self.expiring_keys.remove(tree_key.as_ref()).is_some() {
            let index = self
                .expiration_order
                .iter()
                .enumerate()
                .find_map(|(index, key)| {
                    if tree_key.as_ref() == key {
                        Some(index)
                    } else {
                        None
                    }
                })
                .unwrap();

            changed_first_expiration |= index == 0;
            self.expiration_order.remove(index);
        }

        if changed_first_expiration {
            self.update_background_worker_target();
        }
    }

    #[cfg_attr(feature = "tracing", tracing::instrument(level = "trace", skip(self)))]
    fn execute_get_operation(
        &mut self,
        namespace: Option<&str>,
        key: &str,
        delete: bool,
    ) -> Result<Output, bonsaidb_core::Error> {
        let full_key = full_key(namespace, key);
        let entry = if delete {
            self.remove(full_key).map_err(Error::from)?
        } else {
            self.get(&full_key).map_err(Error::from)?
        };

        Ok(Output::Value(entry.map(|e| e.value)))
    }

    #[cfg_attr(feature = "tracing", tracing::instrument(level = "trace", skip(self)))]
    fn execute_delete_operation(
        &mut self,
        namespace: Option<&str>,
        key: &str,
    ) -> Result<Output, bonsaidb_core::Error> {
        let full_key = full_key(namespace, key);
        let value = self.remove(full_key).map_err(Error::from)?;
        if value.is_some() {
            Ok(Output::Status(KeyStatus::Deleted))
        } else {
            Ok(Output::Status(KeyStatus::NotChanged))
        }
    }

    #[cfg_attr(
        feature = "tracing",
        tracing::instrument(level = "trace", skip(self, amount, saturating, now))
    )]
    fn execute_increment_operation(
        &mut self,
        namespace: Option<&str>,
        key: &str,
        amount: &Numeric,
        saturating: bool,
        now: Timestamp,
    ) -> Result<Output, bonsaidb_core::Error> {
        self.execute_numeric_operation(namespace, key, amount, saturating, now, increment)
    }

    #[cfg_attr(
        feature = "tracing",
        tracing::instrument(level = "trace", skip(self, amount, saturating, now))
    )]
    fn execute_decrement_operation(
        &mut self,
        namespace: Option<&str>,
        key: &str,
        amount: &Numeric,
        saturating: bool,
        now: Timestamp,
    ) -> Result<Output, bonsaidb_core::Error> {
        self.execute_numeric_operation(namespace, key, amount, saturating, now, decrement)
    }

    fn execute_numeric_operation<F: Fn(&Numeric, &Numeric, bool) -> Numeric>(
        &mut self,
        namespace: Option<&str>,
        key: &str,
        amount: &Numeric,
        saturating: bool,
        now: Timestamp,
        op: F,
    ) -> Result<Output, bonsaidb_core::Error> {
        let full_key = full_key(namespace, key);
        let current = self.get(&full_key).map_err(Error::from)?;
        let mut entry = current.unwrap_or(Entry {
            value: Value::Numeric(Numeric::UnsignedInteger(0)),
            expiration: None,
            last_updated: now,
        });

        match entry.value {
            Value::Numeric(existing) => {
                let value = Value::Numeric(op(&existing, amount, saturating).validate()?);
                entry.value = value.clone();

                self.set(full_key, entry);
                Ok(Output::Value(Some(value)))
            }
            Value::Bytes(_) => Err(bonsaidb_core::Error::other(
                "bonsaidb-local",
                "type of stored `Value` is not `Numeric`",
            )),
        }
    }

    #[cfg_attr(feature = "tracing", tracing::instrument(level = "trace", skip(self)))]
    fn remove(&mut self, key: String) -> Result<Option<Entry>, nebari::Error> {
        self.update_key_expiration(&key, None);

        if let Some(dirty_entry) = self.dirty_keys.get_mut(&key) {
            Ok(dirty_entry.take())
        } else if let Some(persisting_entry) = self
            .keys_being_persisted
            .as_ref()
            .and_then(|keys| keys.get(&key))
        {
            self.dirty_keys.insert(key, None);
            Ok(persisting_entry.clone())
        } else {
            // There might be a value on-disk we need to remove.
            let previous_value = Self::retrieve_key_from_disk(&self.roots, &key)?;
            self.dirty_keys.insert(key, None);
            Ok(previous_value)
        }
    }

    #[cfg_attr(feature = "tracing", tracing::instrument(level = "trace", skip(self)))]
    fn get(&self, key: &str) -> Result<Option<Entry>, nebari::Error> {
        if let Some(entry) = self.dirty_keys.get(key) {
            Ok(entry.clone())
        } else if let Some(persisting_entry) = self
            .keys_being_persisted
            .as_ref()
            .and_then(|keys| keys.get(key))
        {
            Ok(persisting_entry.clone())
        } else {
            Self::retrieve_key_from_disk(&self.roots, key)
        }
    }

    fn set(&mut self, key: String, value: Entry) {
        self.dirty_keys.insert(key, Some(value));
    }

    fn replace(&mut self, key: String, value: Entry) -> Result<Option<Entry>, nebari::Error> {
        let mut value = Some(value);
        let map_entry = self.dirty_keys.entry(key);
        if matches!(map_entry, btree_map::Entry::Vacant(_)) {
            // This key is clean, and the caller is expecting the previous
            // value.
            let stored_value = if let Some(persisting_entry) = self
                .keys_being_persisted
                .as_ref()
                .and_then(|keys| keys.get(map_entry.key()))
            {
                persisting_entry.clone()
            } else {
                Self::retrieve_key_from_disk(&self.roots, map_entry.key())?
            };
            map_entry.or_insert(value);
            Ok(stored_value)
        } else {
            // This key is already dirty, we can just replace the value and
            // return the old value.
            map_entry.and_modify(|map_entry| {
                std::mem::swap(&mut value, map_entry);
            });
            Ok(value)
        }
    }

    #[cfg_attr(feature = "tracing", tracing::instrument(level = "trace", skip(roots)))]
    fn retrieve_key_from_disk(
        roots: &Roots<AnyFile>,
        key: &str,
    ) -> Result<Option<Entry>, nebari::Error> {
        roots
            .tree(Unversioned::tree(KEY_TREE))?
            .get(key.as_bytes())
            .map(|current| current.and_then(|current| bincode::deserialize::<Entry>(&current).ok()))
    }

    fn update_background_worker_target(&mut self) {
        let key_expiration_target = self.expiration_order.get(0).map(|key| {
            let expiration_timeout = self.expiring_keys.get(key).unwrap();
            *expiration_timeout
        });
        let now = Timestamp::now();
        let persisting = self.keys_being_persisted.is_some();
        let commit_target = (!persisting)
            .then(|| {
                self.persistence.duration_until_next_commit(
                    self.dirty_keys.len(),
                    (now - self.last_commit).unwrap_or_default(),
                )
            })
            .flatten()
            .map(|duration| now + duration);
        match (commit_target, key_expiration_target) {
            (Some(target), _) | (_, Some(target)) if target <= now => {
                self.background_worker_target
                    .replace(BackgroundWorkerProcessTarget::Now);
            }
            (Some(commit_target), Some(key_target)) => {
                let closest_target = key_target.min(commit_target);
                let new_target = BackgroundWorkerProcessTarget::Timestamp(closest_target);
                let _: Result<_, _> = self.background_worker_target.update(new_target);
            }
            (Some(target), None) | (None, Some(target)) => {
                let _: Result<_, _> = self
                    .background_worker_target
                    .update(BackgroundWorkerProcessTarget::Timestamp(target));
            }
            (None, None) => {
                let _: Result<_, _> = self
                    .background_worker_target
                    .update(BackgroundWorkerProcessTarget::Never);
            }
        }
    }

    fn remove_expired_keys(&mut self, now: Timestamp) {
        while !self.expiration_order.is_empty()
            && self.expiring_keys.get(&self.expiration_order[0]).unwrap() <= &now
        {
            let key = self.expiration_order.pop_front().unwrap();
            self.expiring_keys.remove(&key);
            self.dirty_keys.insert(key, None);
        }
    }

    fn needs_commit(&mut self, now: Timestamp) -> bool {
        if self.keys_being_persisted.is_some() {
            false
        } else {
            let since_last_commit = (now - self.last_commit).unwrap_or_default();
            self.persistence
                .should_commit(self.dirty_keys.len(), since_last_commit)
        }
    }

    fn stage_dirty_keys(&mut self) -> Option<Arc<BTreeMap<String, Option<Entry>>>> {
        if !self.dirty_keys.is_empty() && self.keys_being_persisted.is_none() {
            let keys = Arc::new(std::mem::take(&mut self.dirty_keys));
            self.keys_being_persisted = Some(keys.clone());
            Some(keys)
        } else {
            None
        }
    }

    pub fn commit_dirty_keys(&mut self, state: &Arc<Mutex<KeyValueState>>) -> bool {
        if let Some(keys) = self.stage_dirty_keys() {
            let roots = self.roots.clone();
            let state = state.clone();
            std::thread::Builder::new()
                .name(String::from("keyvalue-persist"))
                .spawn(move || Self::persist_keys(&state, &roots, &keys))
                .unwrap();
            self.last_commit = Timestamp::now();
            true
        } else {
            false
        }
    }

    #[cfg(test)]
    pub fn persistence_watcher(&self) -> Watcher<Timestamp> {
        self.last_persistence.watch()
    }

    #[cfg_attr(feature = "instrument", tracing::instrument(level = "trace", skip_all))]
    fn persist_keys(
        key_value_state: &Arc<Mutex<KeyValueState>>,
        roots: &Roots<AnyFile>,
        keys: &BTreeMap<String, Option<Entry>>,
    ) -> Result<(), bonsaidb_core::Error> {
        let mut transaction = roots
            .transaction(&[Unversioned::tree(KEY_TREE)])
            .map_err(Error::from)?;
        let all_keys = keys
            .keys()
            .map(|key| ArcBytes::from(key.as_bytes().to_vec()))
            .collect();
        let mut changed_keys = Vec::new();
        transaction
            .tree::<Unversioned>(0)
            .unwrap()
            .modify(
                all_keys,
                Operation::CompareSwap(CompareSwap::new(&mut |key, existing_value| {
                    let full_key = std::str::from_utf8(key).unwrap();
                    let (namespace, key) = split_key(full_key).unwrap();

                    if let Some(new_value) = keys.get(full_key).unwrap() {
                        changed_keys.push(ChangedKey {
                            namespace,
                            key,
                            deleted: false,
                        });
                        let bytes = bincode::serialize(new_value).unwrap();
                        nebari::tree::KeyOperation::Set(ArcBytes::from(bytes))
                    } else if existing_value.is_some() {
                        changed_keys.push(ChangedKey {
                            namespace,
                            key,
                            deleted: existing_value.is_some(),
                        });
                        nebari::tree::KeyOperation::Remove
                    } else {
                        nebari::tree::KeyOperation::Skip
                    }
                })),
            )
            .map_err(Error::from)?;

        if !changed_keys.is_empty() {
            transaction
                .entry_mut()
                .set_data(compat::serialize_executed_transaction_changes(
                    &Changes::Keys(changed_keys),
                )?)
                .map_err(Error::from)?;
            transaction.commit().map_err(Error::from)?;
        }

        // If we are shutting down, check if we still have dirty keys.
        let final_keys = {
            let mut state = key_value_state.lock();
            state.last_persistence.replace(Timestamp::now());
            state.keys_being_persisted = None;
            state.update_background_worker_target();
            // This block is a little ugly to avoid having to acquire the lock
            // twice. If we're shutting down and have no dirty keys, we notify
            // the waiting shutdown task. If we have any dirty keys, we wait do
            // to that step because we're going to recurse and reach this spot
            // again.
            if state.shutdown.is_some() {
                let staged_keys = state.stage_dirty_keys();
                if staged_keys.is_none() {
                    let shutdown = state.shutdown.take().unwrap();
                    let _: Result<_, _> = shutdown.send(());
                }
                staged_keys
            } else {
                None
            }
        };
        if let Some(final_keys) = final_keys {
            Self::persist_keys(key_value_state, roots, &final_keys)?;
        }
        Ok(())
    }
}

pub fn background_worker(
    key_value_state: &Weak<Mutex<KeyValueState>>,
    timestamp_receiver: &mut Watcher<BackgroundWorkerProcessTarget>,
    storage_lock: Option<StorageLock>,
) {
    loop {
        let mut perform_operations = false;
        let current_target = *timestamp_receiver.read();
        match current_target {
            // With no target, sleep until we receive a target.
            BackgroundWorkerProcessTarget::Never => {
                if timestamp_receiver.watch().is_err() {
                    break;
                }
            }
            BackgroundWorkerProcessTarget::Timestamp(target) => {
                // With a target, we need to wait to receive a target only as
                // long as there is time remaining.
                let remaining = target - Timestamp::now();
                if let Some(remaining) = remaining {
                    // recv_timeout panics if Instant::checked_add(remaining)
                    // fails. So, we will cap the sleep time at 1 day.
                    let remaining = remaining.min(Duration::from_secs(60 * 60 * 24));
                    match timestamp_receiver.watch_timeout(remaining) {
                        Ok(_) | Err(watchable::TimeoutError::Timeout) => {
                            perform_operations = true;
                        }
                        Err(watchable::TimeoutError::Disconnected) => break,
                    }
                } else {
                    perform_operations = true;
                }
            }
            BackgroundWorkerProcessTarget::Now => {
                perform_operations = true;
            }
        };

        let Some(key_value_state) = key_value_state.upgrade() else { break };

        if perform_operations {
            let mut state = key_value_state.lock();
            let now = Timestamp::now();
            state.remove_expired_keys(now);
            if state.needs_commit(now) {
                state.commit_dirty_keys(&key_value_state);
            }
            state.update_background_worker_target();
        }
    }

    // The key-value store's delayed persistence can cause the key-value storage
    // to be written past when the last reference to the storage is still held.
    // The storage lock being held ensures that another reader/writer doesn't
    // begin accessing this same storage again.
    drop(storage_lock);
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum BackgroundWorkerProcessTarget {
    Now,
    Timestamp(Timestamp),
    Never,
}

#[derive(Debug)]
pub struct ExpirationLoader {
    pub database: Database,
    pub launched_at: Timestamp,
}

impl Keyed<Task> for ExpirationLoader {
    fn key(&self) -> Task {
        Task::ExpirationLoader(self.database.data.name.clone())
    }
}

impl Job for ExpirationLoader {
    type Error = Error;
    type Output = ();

    #[cfg_attr(feature = "tracing", tracing::instrument(level = "trace", skip_all))]
    fn execute(&mut self) -> Result<Self::Output, Self::Error> {
        let database = self.database.clone();
        let launched_at = self.launched_at;

        for ((namespace, key), entry) in database.all_key_value_entries()? {
            if entry.last_updated < launched_at && entry.expiration.is_some() {
                self.database
                    .update_key_expiration(full_key(namespace.as_deref(), &key), entry.expiration);
            }
        }

        self.database
            .storage()
            .instance
            .tasks()
            .mark_key_value_expiration_loaded(self.database.data.name.clone());

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use bonsaidb_core::arc_bytes::serde::Bytes;
    use bonsaidb_core::test_util::{TestDirectory, TimingTest};
    use nebari::io::any::{AnyFile, AnyFileManager};

    use super::*;
    use crate::config::PersistenceThreshold;
    use crate::database::Context;

    fn run_test_with_persistence<
        F: Fn(Context, nebari::Roots<AnyFile>) -> anyhow::Result<()> + Send,
    >(
        name: &str,
        persistence: KeyValuePersistence,
        test_contents: &F,
    ) -> anyhow::Result<()> {
        let dir = TestDirectory::new(name);
        let sled = nebari::Config::new(&dir)
            .file_manager(AnyFileManager::std())
            .open()?;

        let context = Context::new(sled.clone(), persistence, None);

        test_contents(context, sled)?;

        Ok(())
    }

    fn run_test<F: Fn(Context, nebari::Roots<AnyFile>) -> anyhow::Result<()> + Send>(
        name: &str,
        test_contents: F,
    ) -> anyhow::Result<()> {
        run_test_with_persistence(name, KeyValuePersistence::default(), &test_contents)
    }

    #[test]
    fn basic_expiration() -> anyhow::Result<()> {
        run_test("kv-basic-expiration", |context, roots| {
            // Initialize the test state
            let mut persistence_watcher = context.kv_persistence_watcher();
            roots.delete_tree(KEY_TREE)?;
            let tree = roots.tree(Unversioned::tree(KEY_TREE))?;
            tree.set(b"atree\0akey", b"somevalue")?;

            // Expire the existing key
            context.update_key_expiration(
                full_key(Some("atree"), "akey"),
                Some(Timestamp::now() + Duration::from_millis(100)),
            );
            // Wait for persistence.
            persistence_watcher.next_value()?;

            // Verify it is gone.
            assert!(tree.get(b"akey")?.is_none());

            Ok(())
        })
    }

    #[test]
    fn updating_expiration() -> anyhow::Result<()> {
        run_test("kv-updating-expiration", |context, roots| {
            // Initialize the test state
            let mut persistence_watcher = context.kv_persistence_watcher();
            roots.delete_tree(KEY_TREE)?;
            let tree = roots.tree(Unversioned::tree(KEY_TREE))?;
            tree.set(b"atree\0akey", b"somevalue")?;
            let start = Timestamp::now();

            // Set the expiration once.
            context.update_key_expiration(
                full_key(Some("atree"), "akey"),
                Some(start + Duration::from_millis(100)),
            );
            // Set the expiration to a longer value.
            let correct_expiration = start + Duration::from_secs(1);
            context
                .update_key_expiration(full_key(Some("atree"), "akey"), Some(correct_expiration));

            // Wait for persistence, and ensure that the next persistence is
            // after our expiration timestamp.
            assert!(persistence_watcher.next_value()? > correct_expiration);

            // Verify the key is gone now.
            assert_eq!(tree.get(b"atree\0akey")?, None);

            Ok(())
        })
    }

    #[test]
    fn multiple_keys_expiration() -> anyhow::Result<()> {
        run_test("kv-multiple-keys-expiration", |context, roots| {
            // Initialize the test state
            let mut persistence_watcher = context.kv_persistence_watcher();
            roots.delete_tree(KEY_TREE)?;
            let tree = roots.tree(Unversioned::tree(KEY_TREE))?;
            tree.set(b"atree\0akey", b"somevalue")?;
            tree.set(b"atree\0bkey", b"somevalue")?;

            // Expire both keys, one for a shorter time than the other.
            context.update_key_expiration(
                full_key(Some("atree"), "akey"),
                Some(Timestamp::now() + Duration::from_millis(100)),
            );
            context.update_key_expiration(
                full_key(Some("atree"), "bkey"),
                Some(Timestamp::now() + Duration::from_secs(1)),
            );

            // Wait for the first persistence.
            persistence_watcher.next_value()?;
            assert!(tree.get(b"atree\0akey")?.is_none());
            assert!(tree.get(b"atree\0bkey")?.is_some());

            // Wait for the second persistence.
            persistence_watcher.next_value()?;
            assert!(tree.get(b"atree\0bkey")?.is_none());

            Ok(())
        })
    }

    #[test]
    fn clearing_expiration() -> anyhow::Result<()> {
        run_test("kv-clearing-expiration", |sender, sled| {
            loop {
                sled.delete_tree(KEY_TREE)?;
                let tree = sled.tree(Unversioned::tree(KEY_TREE))?;
                tree.set(b"atree\0akey", b"somevalue")?;
                let timing = TimingTest::new(Duration::from_millis(100));
                sender.update_key_expiration(
                    full_key(Some("atree"), "akey"),
                    Some(Timestamp::now() + Duration::from_millis(100)),
                );
                sender.update_key_expiration(full_key(Some("atree"), "akey"), None);
                if timing.elapsed() > Duration::from_millis(100) {
                    // Restart, took too long.
                    continue;
                }
                timing.wait_until(Duration::from_millis(150));
                assert!(tree.get(b"atree\0akey")?.is_some());
                break;
            }

            Ok(())
        })
    }

    #[test]
    fn out_of_order_expiration() -> anyhow::Result<()> {
        run_test("kv-out-of-order-expiration", |context, roots| loop {
            context.update_key_expiration(full_key(Some("atree"), "akey"), None);
            context.update_key_expiration(full_key(Some("atree"), "bkey"), None);
            context.update_key_expiration(full_key(Some("atree"), "ckey"), None);
            let mut persistence_watcher = context.kv_persistence_watcher();
            drop(roots.delete_tree(KEY_TREE));
            let tree = roots.tree(Unversioned::tree(KEY_TREE))?;
            tree.set(b"atree\0akey", b"somevalue")?;
            tree.set(b"atree\0bkey", b"somevalue")?;
            tree.set(b"atree\0ckey", b"somevalue")?;
            let timing = TimingTest::new(Duration::from_millis(100));
            context.update_key_expiration(
                full_key(Some("atree"), "akey"),
                Some(Timestamp::now() + Duration::from_secs(3)),
            );
            context.update_key_expiration(
                full_key(Some("atree"), "ckey"),
                Some(Timestamp::now() + Duration::from_secs(1)),
            );
            context.update_key_expiration(
                full_key(Some("atree"), "bkey"),
                Some(Timestamp::now() + Duration::from_secs(2)),
            );
            persistence_watcher.mark_read();
            if timing.elapsed() > Duration::from_millis(500) {
                println!("Restarting");
                continue;
            }

            // Wait for the first key to expire.
            persistence_watcher
                .watch_timeout(Duration::from_secs(5))
                .unwrap();
            persistence_watcher.mark_read();
            if timing.elapsed() > Duration::from_millis(1500) {
                println!("Restarting");
                continue;
            }
            assert!(tree.get(b"atree\0akey")?.is_some());
            assert!(tree.get(b"atree\0bkey")?.is_some());
            assert!(tree.get(b"atree\0ckey")?.is_none());

            // Wait for the next key to expire.
            persistence_watcher
                .watch_timeout(Duration::from_secs(5))
                .unwrap();
            persistence_watcher.mark_read();
            if timing.elapsed() > Duration::from_millis(2500) {
                println!("Restarting");
                continue;
            }
            assert!(tree.get(b"atree\0akey")?.is_some());
            assert!(tree.get(b"atree\0bkey")?.is_none());

            // Wait for the final key to expire.
            persistence_watcher
                .watch_timeout(Duration::from_secs(5))
                .unwrap();
            if timing.elapsed() > Duration::from_millis(3500) {
                println!("Restarting");
                continue;
            }
            assert!(tree.get(b"atree\0akey")?.is_none());

            return Ok(());
        })
    }

    #[test]
    fn basic_persistence() -> anyhow::Result<()> {
        run_test_with_persistence(
            "kv-basic-persistence",
            KeyValuePersistence::lazy([
                PersistenceThreshold::after_changes(2),
                PersistenceThreshold::after_changes(1).and_duration(Duration::from_secs(2)),
            ]),
            &|context, roots| {
                // Initialize the test state
                let mut persistence_watcher = context.kv_persistence_watcher();
                let tree = roots.tree(Unversioned::tree(KEY_TREE))?;
                let start = Instant::now();
                // Set three keys in quick succession. The first two should
                // persist immediately after the second is set, and the
                // third should show up after 2 seconds.
                context
                    .perform_kv_operation(KeyOperation {
                        namespace: None,
                        key: String::from("key1"),
                        command: Command::Set(SetCommand {
                            value: Value::Bytes(Bytes::default()),
                            expiration: None,
                            keep_existing_expiration: false,
                            check: None,
                            return_previous_value: false,
                        }),
                    })
                    .unwrap();
                context
                    .perform_kv_operation(KeyOperation {
                        namespace: None,
                        key: String::from("key2"),
                        command: Command::Set(SetCommand {
                            value: Value::Bytes(Bytes::default()),
                            expiration: None,
                            keep_existing_expiration: false,
                            check: None,
                            return_previous_value: false,
                        }),
                    })
                    .unwrap();
                context
                    .perform_kv_operation(KeyOperation {
                        namespace: None,
                        key: String::from("key3"),
                        command: Command::Set(SetCommand {
                            value: Value::Bytes(Bytes::default()),
                            expiration: None,
                            keep_existing_expiration: false,
                            check: None,
                            return_previous_value: false,
                        }),
                    })
                    .unwrap();
                // Wait for the first persistence to occur.
                persistence_watcher.next_value()?;

                assert!(tree.get(b"\0key1").unwrap().is_some());
                assert!(tree.get(b"\0key2").unwrap().is_some());
                assert!(tree.get(b"\0key3").unwrap().is_none());

                // Wait for the second persistence
                persistence_watcher.next_value()?;
                assert!(tree.get(b"\0key3").unwrap().is_some());
                // The total operation should have taken *at least* two seconds,
                // since the second persistence should have delayed for two
                // seconds itself.
                assert!(start.elapsed() > Duration::from_secs(2));

                Ok(())
            },
        )
    }

    #[test]
    fn saves_on_drop() -> anyhow::Result<()> {
        let dir = TestDirectory::new("saves-on-drop.bonsaidb");
        let sled = nebari::Config::new(&dir)
            .file_manager(AnyFileManager::std())
            .open()?;
        let tree = sled.tree(Unversioned::tree(KEY_TREE))?;

        let context = Context::new(
            sled,
            KeyValuePersistence::lazy([PersistenceThreshold::after_changes(2)]),
            None,
        );
        context
            .perform_kv_operation(KeyOperation {
                namespace: None,
                key: String::from("key1"),
                command: Command::Set(SetCommand {
                    value: Value::Bytes(Bytes::default()),
                    expiration: None,
                    keep_existing_expiration: false,
                    check: None,
                    return_previous_value: false,
                }),
            })
            .unwrap();
        assert!(tree.get(b"\0key1").unwrap().is_none());
        drop(context);
        // Dropping spawns a task that should persist the keys. Give a moment
        // for the runtime to execute the task.
        std::thread::sleep(Duration::from_millis(100));
        assert!(tree.get(b"\0key1").unwrap().is_some());

        Ok(())
    }
}
