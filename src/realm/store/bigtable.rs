use crate::autogen::google;
use async_trait::async_trait;
use google::bigtable::admin::v2::table::TimestampGranularity;
use google::bigtable::admin::v2::{ColumnFamily, CreateTableRequest, GcRule, Table};
use google::bigtable::v2::column_range::{EndQualifier, StartQualifier};
use google::bigtable::v2::row_range::{
    EndKey::EndKeyClosed, EndKey::EndKeyOpen, StartKey::StartKeyClosed,
};
use google::bigtable::v2::{
    mutate_rows_request, mutation, read_rows_request, row_filter, row_filter::Filter,
    CheckAndMutateRowRequest, ColumnRange, MutateRowRequest, MutateRowResponse, MutateRowsRequest,
    Mutation, ReadRowsRequest, RowFilter, RowRange, RowSet,
};
use http::Uri;
use loam_sdk_core::marshalling;
use std::collections::HashMap;
use std::fmt;
use std::fmt::Write;
use std::ops::Deref;
use std::process::Command;
use std::sync::Mutex;
use std::time::Duration;
use tokio::time::sleep;
use tonic::transport::{Channel, Endpoint};
use tracing::{info, instrument, trace};
use url::Url;

mod mutate;
mod read;

use super::super::merkle::agent::TreeStoreReader;
use hsmcore::hsm::types::{DataHash, EntryHmac, GroupId, HsmId, LogEntry, LogIndex, RecordId};
use hsmcore::merkle::agent::{all_store_key_starts, Node, StoreDelta, StoreKey, TreeStoreError};
use loam_sdk_core::types::RealmId;

use mutate::{mutate_rows, MutateRowsError};
use read::read_rows;

type BigtableTableAdminClient =
    google::bigtable::admin::v2::bigtable_table_admin_client::BigtableTableAdminClient<Channel>;
type BigtableClient =
    google::bigtable::v2::bigtable_client::BigtableClient<tonic::transport::Channel>;

#[derive(clap::Args, Clone, Debug)]
pub struct BigTableArgs {
    /// The name of the GCP project that contains the bigtable instance.
    #[arg(long = "bigtable-project", default_value = "prj")]
    pub project: String,

    /// The name of the bigtable instance to connect to.
    #[arg(long = "bigtable-inst", default_value = "inst")]
    pub inst: String,

    /// The url to the big table emulator [default uses GCP endpoints].
    #[arg(long = "bigtable-url")]
    pub url: Option<Uri>,
}

impl BigTableArgs {
    pub async fn connect_data(&self) -> StoreClient {
        let data_url = match &self.url {
            Some(u) => u.clone(),
            None => Uri::from_static("https://bigtable.googleapis.com"),
        };
        info!(
            inst = self.inst,
            project = self.project,
            %data_url,
            "Connecting to Bigtable Data"
        );
        let instance = Instance {
            project: self.project.clone(),
            instance: self.inst.clone(),
        };
        StoreClient::new(data_url.clone(), instance)
            .await
            .unwrap_or_else(|e| panic!("Unable to connect to Bigtable at `{data_url}`: {e}"))
    }

    pub async fn connect_admin(&self) -> StoreAdminClient {
        let admin_url = match &self.url {
            Some(u) => u.clone(),
            None => Uri::from_static("https://bigtableadmin.googleapis.com"),
        };
        info!(
            inst = self.inst,
            project = self.project,
             %admin_url,
            "Connecting to Bigtable Admin"
        );
        let instance = Instance {
            project: self.project.clone(),
            instance: self.inst.clone(),
        };
        StoreAdminClient::new(admin_url.clone(), instance)
            .await
            .unwrap_or_else(|e| panic!("Unable to connect to Bigtable admin at `{admin_url}`: {e}"))
    }

    pub fn add_to_cmd(&self, cmd: &mut Command) {
        cmd.arg("--bigtable-inst")
            .arg(&self.inst)
            .arg("--bigtable-project")
            .arg(&self.project);
        if let Some(u) = &self.url {
            cmd.arg("--bigtable-url").arg(u.to_string());
        }
    }
}

#[derive(Clone, Debug)]
pub struct Instance {
    pub project: String,
    pub instance: String,
}

impl Instance {
    fn path(&self) -> String {
        format!(
            "projects/{project}/instances/{instance}",
            project = self.project,
            instance = self.instance,
        )
    }
}

fn merkle_table(instance: &Instance, realm: &RealmId) -> String {
    let mut buf = String::new();
    write!(
        buf,
        "projects/{project}/instances/{instance}/tables/",
        project = instance.project,
        instance = instance.instance
    )
    .unwrap();
    for byte in realm.0 {
        write!(buf, "{byte:02x}").unwrap();
    }
    write!(buf, "-merkle").unwrap();
    buf
}

fn merkle_table_brief(realm: &RealmId) -> String {
    let mut buf = String::new();
    for byte in realm.0 {
        write!(buf, "{byte:02x}").unwrap();
    }
    write!(buf, "-merkle").unwrap();
    buf
}

fn log_table(instance: &Instance, realm: &RealmId) -> String {
    let mut buf = String::new();
    write!(
        buf,
        "projects/{project}/instances/{instance}/tables/",
        project = instance.project,
        instance = instance.instance
    )
    .unwrap();
    for byte in realm.0 {
        write!(buf, "{byte:02x}").unwrap();
    }
    write!(buf, "-log").unwrap();
    buf
}

fn log_table_brief(realm: &RealmId) -> String {
    let mut buf = String::new();
    for byte in realm.0 {
        write!(buf, "{byte:02x}").unwrap();
    }
    write!(buf, "-log").unwrap();
    buf
}

fn discovery_table(instance: &Instance) -> String {
    format!(
        "projects/{project}/instances/{instance}/tables/discovery",
        project = instance.project,
        instance = instance.instance
    )
}

fn discovery_table_brief() -> String {
    String::from("discovery")
}

struct DownwardLogIndex(LogIndex);

impl DownwardLogIndex {
    fn bytes(&self) -> [u8; 8] {
        let index: LogIndex = self.0;
        let index: u64 = index.0;
        (u64::MAX - index).to_be_bytes()
    }
}

fn log_key(group: &GroupId, index: LogIndex) -> Vec<u8> {
    (group.0.iter())
        .chain(DownwardLogIndex(index).bytes().iter())
        .cloned()
        .collect()
}

#[derive(Clone)]
pub struct StoreAdminClient {
    // https://cloud.google.com/bigtable/docs/reference/admin/rpc/google.bigtable.admin.v2
    bigtable: BigtableTableAdminClient,
    instance: Instance,
}

impl fmt::Debug for StoreAdminClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StoreAdminClient")
            .field("instance", &self.instance)
            .finish_non_exhaustive()
    }
}

impl StoreAdminClient {
    pub async fn new(url: Uri, instance: Instance) -> Result<Self, tonic::transport::Error> {
        let endpoint = Endpoint::from(url);
        let bigtable = BigtableTableAdminClient::connect(endpoint).await?;
        Ok(Self { bigtable, instance })
    }

    /// Creates a little Bigtable table for service discovery.
    pub async fn initialize_discovery(&self) -> Result<(), tonic::Status> {
        // This is not realm-specific, so it might already exist.
        if let Err(e) = self
            .bigtable
            .clone()
            .create_table(CreateTableRequest {
                parent: self.instance.path(),
                table_id: discovery_table_brief(),
                table: Some(Table {
                    name: String::from(""),
                    cluster_states: HashMap::new(),
                    column_families: HashMap::from([(
                        String::from("f"),
                        ColumnFamily {
                            gc_rule: Some(GcRule { rule: None }),
                        },
                    )]),
                    granularity: TimestampGranularity::Unspecified as i32,
                    restore_info: None,
                    deletion_protection: false,
                }),
                initial_splits: Vec::new(),
            })
            .await
        {
            if e.code() != tonic::Code::AlreadyExists {
                return Err(e);
            }
        }
        Ok(())
    }

    pub async fn initialize_realm(&self, realm: &RealmId) -> Result<(), tonic::Status> {
        let mut bigtable = self.bigtable.clone();

        self.initialize_discovery().await?;

        // Create table for Merkle trees.
        bigtable
            .create_table(CreateTableRequest {
                parent: self.instance.path(),
                table_id: merkle_table_brief(realm),
                table: Some(Table {
                    name: String::from(""),
                    cluster_states: HashMap::new(),
                    column_families: HashMap::from([(
                        String::from("f"),
                        ColumnFamily {
                            gc_rule: Some(GcRule { rule: None }),
                        },
                    )]),
                    granularity: TimestampGranularity::Unspecified as i32,
                    restore_info: None,
                    deletion_protection: false,
                }),
                initial_splits: Vec::new(),
            })
            .await?;

        // Create table for logs.
        bigtable
            .create_table(CreateTableRequest {
                parent: self.instance.path(),
                table_id: log_table_brief(realm),
                table: Some(Table {
                    name: String::from(""),
                    cluster_states: HashMap::new(),
                    column_families: HashMap::from([(
                        String::from("f"),
                        ColumnFamily {
                            gc_rule: Some(GcRule { rule: None }),
                        },
                    )]),
                    granularity: TimestampGranularity::Unspecified as i32,
                    restore_info: None,
                    deletion_protection: false,
                }),
                initial_splits: Vec::new(),
            })
            .await?;

        Ok(())
    }
}

pub struct StoreClient {
    // https://cloud.google.com/bigtable/docs/reference/data/rpc/google.bigtable.v2
    bigtable: BigtableClient,
    instance: Instance,
    last_write: Mutex<Option<(RealmId, GroupId, LogIndex, EntryHmac)>>,
}

impl Clone for StoreClient {
    fn clone(&self) -> Self {
        Self {
            bigtable: self.bigtable.clone(),
            instance: self.instance.clone(),
            last_write: Mutex::new(None),
        }
    }
}

impl fmt::Debug for StoreClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StoreClient")
            .field("instance", &self.instance)
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
pub enum AppendError {
    Grpc(tonic::Status),
    MerkleWrites(google::rpc::Status),
    LogPrecondition,
    MerkleDeletes(google::rpc::Status),
}

#[derive(Debug)]
pub struct Append {
    pub entry: LogEntry,
    pub delta: Option<StoreDelta<DataHash>>,
}

impl StoreClient {
    pub async fn new(url: Uri, instance: Instance) -> Result<Self, tonic::transport::Error> {
        let endpoint = Endpoint::from(url);
        let bigtable = BigtableClient::connect(endpoint).await?;
        Ok(Self {
            bigtable,
            instance,
            last_write: Mutex::new(None),
        })
    }

    #[instrument(level = "trace", skip(self))]
    pub async fn append(
        &self,
        realm: &RealmId,
        group: &GroupId,
        items: &[Append],
    ) -> Result<(), AppendError> {
        assert!(
            !items.is_empty(),
            "append passed empty list of things to append."
        );
        trace!(
            realm = ?realm,
            group = ?group,
            first_index = ?items[0].entry.index,
            items = items.len(),
            "append starting",
        );

        // Make sure the previous log entry exists and matches the expected value.
        if items[0].entry.index != LogIndex::FIRST {
            let prev_index = items[0].entry.index.prev().unwrap();
            let read_log_entry = {
                let last_write = self.last_write.lock().unwrap();
                match last_write.deref() {
                    Some((last_realm, last_group, last_index, last_hmac))
                        if last_realm == realm
                            && last_group == group
                            && *last_index == prev_index =>
                    {
                        if *last_hmac != items[0].entry.prev_hmac {
                            return Err(AppendError::LogPrecondition);
                        }
                        false
                    }
                    _ => true,
                }
            };
            if read_log_entry {
                if let Some(prev) = self
                    .read_log_entry(realm, group, prev_index)
                    .await
                    .expect("TODO")
                {
                    if prev.entry_hmac != items[0].entry.prev_hmac {
                        return Err(AppendError::LogPrecondition);
                    }
                } else {
                    return Err(AppendError::LogPrecondition);
                };
            }
        }

        // Make sure the batch of entries have the expected indexes & hmacs
        let mut prev = &items[0].entry;
        for e in &items[1..] {
            assert_eq!(e.entry.index, prev.index.next());
            assert_eq!(e.entry.prev_hmac, prev.entry_hmac);
            prev = &e.entry;
        }

        let mut bigtable = self.bigtable.clone();

        // Write new Merkle nodes.
        let new_merkle_entries = items
            .iter()
            .filter(|i| i.delta.is_some())
            .flat_map(|i| i.delta.as_ref().unwrap().add.iter())
            .map(|(key, value)| mutate_rows_request::Entry {
                row_key: key.store_key().into_bytes(),
                mutations: vec![Mutation {
                    mutation: Some(mutation::Mutation::SetCell(mutation::SetCell {
                        family_name: String::from("f"),
                        column_qualifier: b"n".to_vec(),
                        timestamp_micros: -1,
                        // TODO: unnecessarily wraps the leaf node values.
                        value: marshalling::to_vec(value).expect("TODO"),
                    })),
                }],
            })
            .collect::<Vec<_>>();

        if !new_merkle_entries.is_empty() {
            mutate_rows(
                &mut bigtable,
                MutateRowsRequest {
                    table_name: merkle_table(&self.instance, realm),
                    app_profile_id: String::new(),
                    entries: new_merkle_entries,
                },
            )
            .await
            .map_err(|e| match e {
                MutateRowsError::Tonic(e) => AppendError::Grpc(e),
                MutateRowsError::Mutation(e) => AppendError::MerkleWrites(e),
            })?;
        }

        // Append the new row but only if it doesn't yet exist.
        let append_response = bigtable
            .check_and_mutate_row(CheckAndMutateRowRequest {
                table_name: log_table(&self.instance, realm),
                app_profile_id: String::new(),
                row_key: log_key(group, items[0].entry.index),
                predicate_filter: None, // checks for any value
                true_mutations: Vec::new(),
                false_mutations: items
                    .iter()
                    .map(|i| Mutation {
                        mutation: Some(mutation::Mutation::SetCell(mutation::SetCell {
                            family_name: String::from("f"),
                            column_qualifier: DownwardLogIndex(i.entry.index).bytes().to_vec(),
                            timestamp_micros: -1,
                            value: marshalling::to_vec(&i.entry).expect("TODO"),
                        })),
                    })
                    .collect(),
            })
            .await
            .map_err(AppendError::Grpc)?
            .into_inner();
        if append_response.predicate_matched {
            return Err(AppendError::LogPrecondition);
        }

        let last = items.last().unwrap();
        *self.last_write.lock().unwrap() = Some((
            *realm,
            *group,
            last.entry.index,
            last.entry.entry_hmac.clone(),
        ));

        // Delete obsolete Merkle nodes. These deletes are deferred a bit so
        // that slow concurrent readers can still access them.
        let to_remove = items
            .iter()
            .filter(|i| i.delta.is_some())
            .flat_map(|i| i.delta.as_ref().unwrap().remove.iter())
            .map(|k| k.store_key())
            .collect::<Vec<_>>();

        if !to_remove.is_empty() {
            let store = self.clone();
            let realm = *realm;
            tokio::spawn(async move {
                sleep(Duration::from_secs(5)).await;
                store.remove(&realm, to_remove).await.expect("TODO");
            });
        }

        trace!("append succeeded");
        Ok(())
    }

    #[instrument(level = "trace", skip(self))]
    async fn remove(
        &self,
        realm: &RealmId,
        to_remove: Vec<StoreKey>,
    ) -> Result<(), MutateRowsError> {
        let mut bigtable = self.bigtable.clone();
        mutate_rows(
            &mut bigtable,
            MutateRowsRequest {
                table_name: merkle_table(&self.instance, realm),
                app_profile_id: String::new(),
                entries: to_remove
                    .into_iter()
                    .map(|key| mutate_rows_request::Entry {
                        row_key: key.into_bytes(),
                        mutations: vec![Mutation {
                            mutation: Some(mutation::Mutation::DeleteFromRow(
                                mutation::DeleteFromRow {},
                            )),
                        }],
                    })
                    .collect(),
            },
        )
        .await
    }

    #[instrument(level = "trace", skip(self))]
    pub async fn read_log_entry(
        &self,
        realm: &RealmId,
        group: &GroupId,
        index: LogIndex,
    ) -> Result<Option<LogEntry>, tonic::Status> {
        let rows = read_rows(
            &mut self.bigtable.clone(),
            ReadRowsRequest {
                table_name: log_table(&self.instance, realm),
                app_profile_id: String::new(),
                rows: Some(RowSet {
                    row_keys: Vec::new(),
                    row_ranges: vec![RowRange {
                        start_key: Some(StartKeyClosed(log_key(group, index))),
                        end_key: Some(EndKeyClosed(log_key(group, LogIndex::FIRST))),
                    }],
                }),
                filter: Some(RowFilter {
                    filter: Some(Filter::ColumnRangeFilter(ColumnRange {
                        family_name: String::from("f"),
                        start_qualifier: Some(StartQualifier::StartQualifierClosed(
                            DownwardLogIndex(index).bytes().to_vec(),
                        )),
                        end_qualifier: Some(EndQualifier::EndQualifierClosed(
                            DownwardLogIndex(index).bytes().to_vec(),
                        )),
                    })),
                }),
                rows_limit: 1,
                request_stats_view: read_rows_request::RequestStatsView::RequestStatsNone.into(),
            },
        )
        .await?;

        let entry: Option<LogEntry> = rows.into_iter().next().and_then(|(_key, cells)| {
            cells
                .into_iter()
                .find(|cell| cell.family == "f")
                .map(|cell| marshalling::from_slice(&cell.value).expect("TODO"))
        });
        if let Some(e) = &entry {
            assert_eq!(e.index, index);
        }
        trace!(
            realm = ?realm,
            group = ?group,
            index = ?index,
            entry = ?entry,
            "read_log_entry completed",
        );
        Ok(entry)
    }

    #[instrument(level = "trace", skip(self))]
    pub async fn read_last_log_entry(
        &self,
        realm: &RealmId,
        group: &GroupId,
    ) -> Result<Option<LogEntry>, tonic::Status> {
        trace!(?realm, ?group, "read_last_log_entry starting");

        let rows = read_rows(
            &mut self.bigtable.clone(),
            ReadRowsRequest {
                table_name: log_table(&self.instance, realm),
                app_profile_id: String::new(),
                rows: Some(RowSet {
                    row_keys: Vec::new(),
                    row_ranges: vec![RowRange {
                        start_key: Some(StartKeyClosed(log_key(group, LogIndex(u64::MAX)))),
                        end_key: Some(EndKeyClosed(log_key(group, LogIndex::FIRST))),
                    }],
                }),
                filter: Some(RowFilter {
                    filter: Some(Filter::CellsPerRowLimitFilter(1)),
                }),
                rows_limit: 1,
                request_stats_view: read_rows_request::RequestStatsView::RequestStatsNone.into(),
            },
        )
        .await?;

        let entry = rows.into_iter().next().and_then(|(_key, cells)| {
            cells
                .into_iter()
                .find(|cell| cell.family == "f")
                .map(|cell| marshalling::from_slice(&cell.value).expect("TODO"))
        });

        trace!(?realm, ?group, ?entry, "read_last_log_entry completed");
        Ok(entry)
    }

    #[instrument(level = "trace", skip(self))]
    pub async fn get_addresses(&self) -> Result<Vec<(HsmId, Url)>, tonic::Status> {
        let rows = read_rows(
            &mut self.bigtable.clone(),
            ReadRowsRequest {
                table_name: discovery_table(&self.instance),
                app_profile_id: String::new(),
                rows: None, // read all rows
                filter: Some(RowFilter {
                    filter: Some(row_filter::Filter::CellsPerColumnLimitFilter(1)),
                }),
                rows_limit: 0,
                request_stats_view: read_rows_request::RequestStatsView::RequestStatsNone.into(),
            },
        )
        .await?;

        let addresses: Vec<(HsmId, Url)> = rows
            .into_iter()
            .filter_map(|(row_key, cells)| {
                cells
                    .into_iter()
                    .find(|cell| cell.family == "f" && cell.qualifier == b"a")
                    .and_then(|cell| String::from_utf8(cell.value).ok())
                    .and_then(|url| Url::parse(&url).ok())
                    .map(|url| {
                        let mut hsm = HsmId([0u8; 16]);
                        hsm.0.copy_from_slice(&row_key.0);
                        (hsm, url)
                    })
            })
            .collect();

        trace!(
            num_addresses = addresses.len(),
            first_address = ?addresses
                .first()
                .map(|(hsm, url)| (hsm, url.as_str())),
            "get_addresses completed"
        );

        Ok(addresses)
    }

    #[instrument(level = "trace", skip(self))]
    pub async fn set_address(&self, hsm: &HsmId, address: &Url) -> Result<(), tonic::Status> {
        trace!(?hsm, address = address.as_str(), "set_address starting");
        let MutateRowResponse { /* empty */ } = self
            .bigtable
            .clone()
            .mutate_row(MutateRowRequest {
                table_name: discovery_table(&self.instance),
                app_profile_id: String::new(),
                row_key: hsm.0.to_vec(),
                mutations: vec![Mutation {
                    mutation: Some(mutation::Mutation::SetCell(mutation::SetCell {
                        family_name: String::from("f"),
                        column_qualifier: b"a".to_vec(),
                        timestamp_micros: -1,
                        value: address.as_str().as_bytes().to_vec(),
                    })),
                }],
            })
            .await?
            .into_inner();
        trace!(?hsm, address = address.as_str(), "set_address completed");
        Ok(())
    }
}

#[async_trait]
impl TreeStoreReader<DataHash> for StoreClient {
    #[instrument(level = "trace", skip(self))]
    async fn path_lookup(
        &self,
        realm: &RealmId,
        record_id: &RecordId,
    ) -> Result<HashMap<DataHash, Node<DataHash>>, TreeStoreError> {
        trace!(realm = ?realm, record = ?record_id, "path_lookup starting");

        let rows = read_rows(
            &mut self.bigtable.clone(),
            ReadRowsRequest {
                table_name: merkle_table(&self.instance, realm),
                app_profile_id: String::new(),
                rows: Some(RowSet {
                    row_keys: Vec::new(),
                    row_ranges: all_store_key_starts(record_id)
                        .into_iter()
                        .map(|prefix| RowRange {
                            end_key: Some(EndKeyOpen(prefix.next().into_bytes())),
                            start_key: Some(StartKeyClosed(prefix.into_bytes())),
                        })
                        .collect(),
                }),
                filter: Some(RowFilter {
                    filter: Some(row_filter::Filter::CellsPerColumnLimitFilter(1)),
                }),
                rows_limit: 0,
                request_stats_view: read_rows_request::RequestStatsView::RequestStatsNone.into(),
            },
        )
        .await
        .map_err(|e| TreeStoreError::Network(e.to_string()))?;

        let nodes: HashMap<DataHash, Node<DataHash>> = rows
            .into_iter()
            .map(|(row_key, cells)| {
                let (_, hash) = StoreKey::parse(&row_key.0).unwrap();
                let node: Node<DataHash> = marshalling::from_slice(
                    &cells
                        .into_iter()
                        .find(|cell| cell.family == "f" && cell.qualifier == b"n")
                        .expect("every Merkle row should contain a node value")
                        .value,
                )
                .expect("TODO");
                (hash, node)
            })
            .collect();

        trace!(realm = ?realm, record = ?record_id, nodes = nodes.len(), "path_lookup completed");
        Ok(nodes)
    }

    #[instrument(level = "trace", skip(self))]
    async fn read_node(
        &self,
        realm: &RealmId,
        key: StoreKey,
    ) -> Result<Node<DataHash>, TreeStoreError> {
        trace!(realm = ?realm, key = ?key, "read_node starting");

        let rows = read_rows(
            &mut self.bigtable.clone(),
            ReadRowsRequest {
                table_name: merkle_table(&self.instance, realm),
                app_profile_id: String::new(),
                rows: Some(RowSet {
                    row_keys: vec![key.clone().into_bytes()],
                    row_ranges: Vec::new(),
                }),
                filter: Some(RowFilter {
                    filter: Some(row_filter::Filter::CellsPerColumnLimitFilter(1)),
                }),
                rows_limit: 0,
                request_stats_view: read_rows_request::RequestStatsView::RequestStatsNone.into(),
            },
        )
        .await
        .map_err(|e| TreeStoreError::Network(e.to_string()))?;

        let node = match rows.into_iter().next().and_then(|(_key, cells)| {
            cells
                .into_iter()
                .find(|cell| cell.family == "f" && cell.qualifier == b"n")
                .map(|cell| marshalling::from_slice(&cell.value).expect("TODO"))
                .expect("every Merkle row should contain a node value")
        }) {
            Some(node) => Ok(node),
            None => Err(TreeStoreError::MissingNode),
        };

        trace!(realm = ?realm, key = ?key, ok = node.is_ok(), "read_node completed");
        node
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const REALM1: RealmId = RealmId([
        0x66, 0x80, 0x13, 0x4b, 0xf4, 0x5d, 0xc9, 0x3f, 0xce, 0xee, 0xcd, 0x03, 0xe5, 0x38, 0xc8,
        0x9f,
    ]);
    const GROUP1: GroupId = GroupId([
        0x0d, 0xbb, 0x03, 0x61, 0xb0, 0xc3, 0x23, 0xdd, 0xeb, 0xa3, 0x4f, 0x4d, 0x02, 0x3a, 0xbb,
        0x53,
    ]);

    #[test]
    fn test_merkle_table() {
        let instance = Instance {
            project: String::from("prj1"),
            instance: String::from("inst2"),
        };
        let expected =
            "projects/prj1/instances/inst2/tables/6680134bf45dc93fceeecd03e538c89f-merkle";
        assert_eq!(merkle_table(&instance, &REALM1), expected);
        assert_eq!(
            format!("{}/tables/{}", instance.path(), merkle_table_brief(&REALM1)),
            expected
        );
    }

    #[test]
    fn test_log_table() {
        let instance = Instance {
            project: String::from("prj1"),
            instance: String::from("inst2"),
        };
        let realm = RealmId([0xca; 16]);
        let expected = "projects/prj1/instances/inst2/tables/cacacacacacacacacacacacacacacaca-log";
        assert_eq!(log_table(&instance, &realm), expected);
        assert_eq!(
            format!("{}/tables/{}", instance.path(), log_table_brief(&realm)),
            expected
        );
    }

    #[test]
    fn test_discovery_table() {
        let instance = Instance {
            project: String::from("prj1"),
            instance: String::from("inst2"),
        };
        let expected = "projects/prj1/instances/inst2/tables/discovery";
        assert_eq!(discovery_table(&instance), expected);
        assert_eq!(
            format!("{}/tables/{}", instance.path(), discovery_table_brief()),
            expected
        );
    }

    #[test]
    fn test_download_logindex() {
        assert_eq!(
            [0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xfe],
            DownwardLogIndex(LogIndex(1)).bytes()
        );
        assert_eq!(
            [0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xfd],
            DownwardLogIndex(LogIndex(2)).bytes()
        );
    }

    #[test]
    fn test_log_key() {
        assert_eq!(
            log_key(&GROUP1, LogIndex(12943236441930260757)),
            vec![
                0x0d, 0xbb, 0x03, 0x61, 0xb0, 0xc3, 0x23, 0xdd, 0xeb, 0xa3, 0x4f, 0x4d, 0x02, 0x3a,
                0xbb, 0x53, 0x4c, 0x60, 0x63, 0x08, 0x42, 0xdb, 0x1e, 0xea
            ]
        );
    }
}
