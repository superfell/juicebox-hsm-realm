use chrono::{DateTime, Datelike, Months, Timelike, Utc};
use google::bigtable::admin::v2::gc_rule::Rule;
use google::bigtable::admin::v2::table::TimestampGranularity;
use google::bigtable::admin::v2::{gc_rule, ColumnFamily, CreateTableRequest, GcRule, Table};
use google::bigtable::v2::column_range::{EndQualifier, StartQualifier};
use google::bigtable::v2::mutate_rows_request::Entry;
use google::bigtable::v2::mutation::{self, SetCell};
use google::bigtable::v2::read_rows_request::RequestStatsView::RequestStatsNone;
use google::bigtable::v2::row_filter::{Chain, Filter, Interleave};
use google::bigtable::v2::value_range::{EndValue, StartValue};
use google::bigtable::v2::{
    ColumnRange, MutateRowsRequest, Mutation, ReadRowsRequest, RowFilter, TimestampRange,
    ValueRange,
};
use hsm_api::RecordId;
use std::collections::HashMap;
use std::time::SystemTime;
use tracing::warn;

use super::mutate::{mutate_rows, MutateRowsError};
use super::read::read_rows_stream;
use super::{BigtableTableAdminClient, Instance, StoreClient};
use juicebox_realm_api::types::RealmId;

const FAMILY: &str = "f";
const EVENT_COL: &[u8] = b"e";

fn tenant_user_table(instance: &Instance, realm: &RealmId) -> String {
    format!(
        "{path}/tables/{table}",
        path = instance.path(),
        table = tenant_user_table_brief(realm),
    )
}

fn tenant_user_table_brief(realm: &RealmId) -> String {
    format!("{realm:?}-users")
}

pub(crate) async fn initialize(
    mut bigtable: BigtableTableAdminClient,
    instance: &Instance,
    realm: &RealmId,
) -> Result<(), tonic::Status> {
    // We keep a cell for every event. The GC rule ensures we keep at least 100
    // days worth of events, and at least the latest 2 events.
    bigtable
        .create_table(CreateTableRequest {
            parent: instance.path(),
            table_id: tenant_user_table_brief(realm),
            table: Some(Table {
                name: String::from(""),
                cluster_states: HashMap::new(),
                column_families: HashMap::from([(
                    FAMILY.to_string(),
                    ColumnFamily {
                        gc_rule: Some(GcRule {
                            rule: Some(Rule::Intersection(gc_rule::Intersection {
                                rules: vec![
                                    GcRule {
                                        rule: Some(Rule::MaxNumVersions(2)),
                                    },
                                    GcRule {
                                        rule: Some(Rule::MaxAge(prost_types::Duration {
                                            seconds: 60 * 60 * 24 * 100,
                                            nanos: 0,
                                        })),
                                    },
                                ],
                            })),
                        }),
                    },
                )]),
                granularity: TimestampGranularity::Unspecified as i32,
                restore_info: None,
                change_stream_config: None,
                deletion_protection: false,
            }),
            initial_splits: Vec::new(),
        })
        .await?;
    Ok(())
}

#[derive(Clone, Debug)]
pub struct UserAccounting {
    pub tenant: String,
    pub id: RecordId,
    pub when: SystemTime,
    pub event: UserAccountingEvent,
}

#[derive(Clone, Copy, Debug)]
pub enum UserAccountingEvent {
    // An existing secret was deleted. Note that the count query depends on this
    // only being recorded for secret to no secret transitions, and not just the
    // fact that delete was called. i.e. This implies that a secret existed
    // before this event occurred.
    SecretDeleted,
    // A secret was registered. Unlike SecretDeleted, this implies nothing about
    // the state prior to this event.
    SecretRegistered,
}

impl UserAccountingEvent {
    fn as_vec(&self) -> Vec<u8> {
        match *self {
            UserAccountingEvent::SecretDeleted => vec![0],
            UserAccountingEvent::SecretRegistered => vec![1],
        }
    }
}

impl StoreClient {
    // Persist user accounting events to the relevant realm table. records must
    // be in ascending timestamp order.
    pub async fn write_user_accounting(
        &self,
        realm: &RealmId,
        records: &[UserAccounting],
    ) -> Result<(), MutateRowsError> {
        assert!(
            records.windows(2).all(|pair| pair[0].when <= pair[1].when),
            "records should be in ascending timestamp order"
        );

        // Each tenant+recordId gets their own row. There's a cell in the row
        // for each event, with a timestamp that is rounded down to midnight.
        // (to stop lots of cells potentially accumulating).
        let mut bigtable = self.bigtable.clone();
        mutate_rows(
            &mut bigtable,
            MutateRowsRequest {
                table_name: tenant_user_table(&self.instance, realm),
                app_profile_id: String::from(""),
                entries: records
                    .iter()
                    .map(|u| Entry {
                        row_key: make_row_key(&u.tenant, &u.id),
                        mutations: vec![Mutation {
                            mutation: Some(mutation::Mutation::SetCell(SetCell {
                                family_name: FAMILY.to_string(),
                                column_qualifier: EVENT_COL.to_vec(),
                                timestamp_micros: to_day_micros(u.when),
                                value: u.event.as_vec(),
                            })),
                        }],
                    })
                    .collect(),
            },
        )
        .await
    }

    pub async fn count_realm_users(
        &self,
        realm: &RealmId,
        when: SystemTime,
    ) -> Result<RealmUserSummary, tonic::Status> {
        let n = DateTime::<Utc>::from(when);
        let start = n
            .with_day(1)
            .unwrap()
            .with_hour(0)
            .unwrap()
            .with_minute(0)
            .unwrap()
            .with_second(0)
            .unwrap()
            .with_nanosecond(0)
            .unwrap();

        let end = start.checked_add_months(Months::new(1)).unwrap();
        let f = Filter::Chain(Chain {
            filters: vec![
                // Just the EVENT_COL cells.
                RowFilter {
                    filter: Some(Filter::ColumnRangeFilter(ColumnRange {
                        family_name: FAMILY.to_string(),
                        start_qualifier: Some(StartQualifier::StartQualifierClosed(
                            EVENT_COL.to_vec(),
                        )),
                        end_qualifier: Some(EndQualifier::EndQualifierClosed(EVENT_COL.to_vec())),
                    })),
                },
                // Upto the end of the date range we're interested in.
                RowFilter {
                    filter: Some(Filter::TimestampRangeFilter(TimestampRange {
                        start_timestamp_micros: 0,
                        end_timestamp_micros: end.timestamp_micros(),
                    })),
                },
                // Just the most recent one (in the above date range)
                RowFilter {
                    filter: Some(Filter::CellsPerColumnLimitFilter(1)),
                },
                RowFilter {
                    // value=[registered] || timestamp >= period_start
                    filter: Some(Filter::Interleave(Interleave {
                        filters: vec![
                            RowFilter {
                                filter: Some(Filter::ValueRangeFilter(ValueRange {
                                    start_value: Some(StartValue::StartValueClosed(
                                        UserAccountingEvent::SecretRegistered.as_vec(),
                                    )),
                                    end_value: Some(EndValue::EndValueClosed(
                                        UserAccountingEvent::SecretRegistered.as_vec(),
                                    )),
                                })),
                            },
                            RowFilter {
                                filter: Some(Filter::TimestampRangeFilter(TimestampRange {
                                    start_timestamp_micros: start.timestamp_micros(),
                                    end_timestamp_micros: 0,
                                })),
                            },
                        ],
                    })),
                },
                // If both the filters in the interleave are true, then there is
                // 2 copies of the cell at this point. Filter it back down to
                // one.
                RowFilter {
                    filter: Some(Filter::CellsPerColumnLimitFilter(1)),
                },
                // We don't care about the value, just the existence of a row
                // that passes this filter.
                RowFilter {
                    filter: Some(Filter::StripValueTransformer(true)),
                },
            ],
        });
        let read_req = ReadRowsRequest {
            table_name: tenant_user_table(&self.instance, realm),
            app_profile_id: String::new(),
            rows: None,
            filter: Some(RowFilter { filter: Some(f) }),
            rows_limit: 0,
            request_stats_view: RequestStatsNone.into(),
            reversed: false,
        };
        let mut bigtable = self.bigtable.clone();
        let mut results = Vec::new();
        match read_rows_stream(&mut bigtable, read_req, |key, _cells| {
            if let Some(t) = parse_tenant(&key.0) {
                match results.last_mut() {
                    Some((last_tenant, count)) if last_tenant == t => *count += 1,
                    None | Some(_) => results.push((t.to_string(), 1)),
                }
            } else {
                warn!(key=?key, "invalid row key, expecting tenant:recordId")
            }
        })
        .await
        {
            Err(err) => {
                warn!(?err, "couldn't read from bigtable");
                Err(err)
            }
            Ok(_) => Ok(RealmUserSummary {
                start: start.into(),
                end: end.into(),
                tenant_user_counts: results,
            }),
        }
    }
}

pub struct RealmUserSummary {
    pub start: SystemTime,
    pub end: SystemTime,
    pub tenant_user_counts: Vec<(String, usize)>,
}

fn make_row_key(tenant: &str, id: &RecordId) -> Vec<u8> {
    use std::fmt::Write;
    let mut k = String::with_capacity(tenant.len() + 1 + (RecordId::NUM_BYTES * 2));
    k.push_str(tenant);
    k.push(':');
    for byte in &id.0 {
        write!(k, "{byte:02x}").unwrap();
    }
    k.into_bytes()
}

fn parse_tenant(row_key: &[u8]) -> Option<&str> {
    const RID_LEN: usize = RecordId::NUM_BYTES * 2;

    if row_key.len() > RID_LEN + 1 {
        let idx = row_key.len() - RID_LEN - 1;
        if row_key[idx] == b':' {
            Some(std::str::from_utf8(&row_key[..idx]).unwrap())
        } else {
            None
        }
    } else {
        None
    }
}

// rounds the supplied time down to midnight and returns the number of micros
// since the EPOCH for that time.
fn to_day_micros(t: SystemTime) -> i64 {
    DateTime::<Utc>::from(t)
        .with_hour(0)
        .unwrap()
        .with_minute(0)
        .unwrap()
        .with_second(0)
        .unwrap()
        .with_nanosecond(0)
        .unwrap()
        .timestamp_micros()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, SystemTime};

    #[test]
    fn day_micros() {
        // Sept 5 2023, 10:18 PDT. / 17:18 UTC
        let t = SystemTime::UNIX_EPOCH + Duration::from_secs(1693934286);
        assert_eq!(1693872000000000, to_day_micros(t));
        assert_eq!(
            1693872000000000,
            to_day_micros(t + Duration::try_from_secs_f64(3600.2).unwrap())
        );
        assert_eq!(
            (1693872000000000 - Duration::from_secs(60 * 60 * 24).as_micros()) as i64,
            to_day_micros(t - Duration::from_secs(60 * 60 * 18))
        );
        assert_eq!(0, to_day_micros(t) % (60 * 60 * 24 * 1_000_000));
        assert_eq!(
            0,
            to_day_micros(SystemTime::now()) % (60 * 60 * 24 * 1_000_000)
        );
    }

    #[test]
    fn row_key() {
        let k = make_row_key("bob", &RecordId([15; 32]));
        assert_eq!(
            "bob:0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f".as_bytes(),
            &k
        );
        assert_eq!(Some("bob"), parse_tenant(&k));
        assert_eq!(
            Some("b"),
            parse_tenant(b"b:ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff")
        );
        assert!(parse_tenant(b"bob:not a record id").is_none());
        assert!(parse_tenant(
            b"bob0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f"
        )
        .is_none());
        assert!(
            parse_tenant(b"0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f")
                .is_none()
        );
        assert!(
            parse_tenant(b":0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f")
                .is_none()
        );
        assert!(parse_tenant(b"bob").is_none());
    }
}
