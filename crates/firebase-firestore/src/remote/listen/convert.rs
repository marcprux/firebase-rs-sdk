//! Conversions between the crate's Firestore model and the generated `google.firestore.v1` protos.
//!
//! One-shot reads and writes speak proto-over-JSON through
//! [`JsonProtoSerializer`](crate::remote::serializer::JsonProtoSerializer); the `Listen`
//! stream speaks binary protobuf, so the same values need a second encoding.

use std::collections::BTreeMap;

use prost_types::Timestamp as ProtoTimestamp;

use crate::api::query::{Bound, FieldFilter, Filter, QueryDefinition};
use crate::error::{internal_error, invalid_argument, FirestoreResult};
use crate::model::{GeoPoint, Timestamp};
use crate::remote::proto::google::firestore::v1 as fs;
use crate::remote::serializer::JsonProtoSerializer;
use crate::remote::watch_change::WatchDocument;
use crate::value::{BytesValue, FirestoreValue, MapValue, ValueKind};

/// Encodes a value for the wire. Sentinels (`serverTimestamp`, `increment`, ...) never travel in a
/// listen request: they are write-side transforms.
pub(crate) fn value_to_proto(value: &FirestoreValue) -> FirestoreResult<fs::Value> {
    let value_type = match value.kind() {
        ValueKind::Null => fs::value::ValueType::NullValue(0),
        ValueKind::Boolean(flag) => fs::value::ValueType::BooleanValue(*flag),
        ValueKind::Integer(number) => fs::value::ValueType::IntegerValue(*number),
        ValueKind::Double(number) => fs::value::ValueType::DoubleValue(*number),
        ValueKind::Timestamp(timestamp) => fs::value::ValueType::TimestampValue(timestamp_to_proto(timestamp)),
        ValueKind::String(text) => fs::value::ValueType::StringValue(text.clone()),
        ValueKind::Bytes(bytes) => fs::value::ValueType::BytesValue(bytes.as_slice().to_vec()),
        ValueKind::Reference(name) => fs::value::ValueType::ReferenceValue(name.clone()),
        ValueKind::GeoPoint(point) => {
            fs::value::ValueType::GeoPointValue(crate::remote::proto::google::r#type::LatLng {
                latitude: point.latitude(),
                longitude: point.longitude(),
            })
        }
        ValueKind::Array(array) => fs::value::ValueType::ArrayValue(fs::ArrayValue {
            values: array
                .values()
                .iter()
                .map(value_to_proto)
                .collect::<FirestoreResult<Vec<_>>>()?,
        }),
        ValueKind::Map(map) => fs::value::ValueType::MapValue(map_to_proto(map)?),
        ValueKind::Sentinel(_) => {
            return Err(invalid_argument("Sentinel values cannot be used in a query or listen request"))
        }
    };

    Ok(fs::Value {
        value_type: Some(value_type),
    })
}

pub(crate) fn map_to_proto(map: &MapValue) -> FirestoreResult<fs::MapValue> {
    let mut fields = BTreeMap::new();
    for (key, value) in map.fields() {
        fields.insert(key.clone(), value_to_proto(value)?);
    }
    Ok(fs::MapValue {
        fields: fields.into_iter().collect(),
    })
}

/// Decodes a value that came back on the listen stream.
pub(crate) fn value_from_proto(value: &fs::Value) -> FirestoreResult<FirestoreValue> {
    let Some(value_type) = value.value_type.as_ref() else {
        return Ok(FirestoreValue::null());
    };

    let decoded = match value_type {
        fs::value::ValueType::NullValue(_) => FirestoreValue::null(),
        fs::value::ValueType::BooleanValue(flag) => FirestoreValue::from_bool(*flag),
        fs::value::ValueType::IntegerValue(number) => FirestoreValue::from_integer(*number),
        fs::value::ValueType::DoubleValue(number) => FirestoreValue::from_double(*number),
        fs::value::ValueType::TimestampValue(timestamp) => {
            FirestoreValue::from_timestamp(timestamp_from_proto(timestamp))
        }
        fs::value::ValueType::StringValue(text) => FirestoreValue::from_string(text.clone()),
        fs::value::ValueType::BytesValue(bytes) => FirestoreValue::from_bytes(BytesValue::new(bytes.to_vec())),
        fs::value::ValueType::ReferenceValue(name) => FirestoreValue::from_reference(name.clone()),
        fs::value::ValueType::GeoPointValue(point) => {
            FirestoreValue::from_geo_point(GeoPoint::new(point.latitude, point.longitude)?)
        }
        fs::value::ValueType::ArrayValue(array) => {
            let values = array
                .values
                .iter()
                .map(value_from_proto)
                .collect::<FirestoreResult<Vec<_>>>()?;
            FirestoreValue::from_array(values)
        }
        fs::value::ValueType::MapValue(map) => FirestoreValue::from_map(map_from_proto(map)?.into_fields()),
        // Pipeline-only value kinds (field/variable references, functions, lists); documents
        // stored in Firestore never contain them.
        other => {
            return Err(internal_error(format!(
                "unsupported Firestore value on the listen stream: {other:?}"
            )))
        }
    };

    Ok(decoded)
}

pub(crate) fn map_from_proto(map: &fs::MapValue) -> FirestoreResult<MapValue> {
    let mut fields = BTreeMap::new();
    for (key, value) in &map.fields {
        fields.insert(key.clone(), value_from_proto(value)?);
    }
    Ok(MapValue::new(fields))
}

pub(crate) fn timestamp_to_proto(timestamp: &Timestamp) -> ProtoTimestamp {
    ProtoTimestamp {
        seconds: timestamp.seconds,
        nanos: timestamp.nanos,
    }
}

pub(crate) fn timestamp_from_proto(timestamp: &ProtoTimestamp) -> Timestamp {
    Timestamp::new(timestamp.seconds, timestamp.nanos)
}

/// Turns a `Document` from the stream into the watch model the aggregator consumes.
pub(crate) fn document_from_proto(
    serializer: &JsonProtoSerializer,
    document: &fs::Document,
) -> FirestoreResult<WatchDocument> {
    let key = serializer.document_key_from_name(&document.name)?;
    let mut fields = BTreeMap::new();
    for (name, value) in &document.fields {
        fields.insert(name.clone(), value_from_proto(value)?);
    }

    Ok(WatchDocument {
        key,
        fields: MapValue::new(fields),
        update_time: document.update_time.as_ref().map(timestamp_from_proto),
        create_time: document.create_time.as_ref().map(timestamp_from_proto),
    })
}

/// Encodes a query the same way [`encode_structured_query`](crate::remote::structured_query)
/// does for REST, but into the protobuf representation the `Listen` RPC expects.
pub(crate) fn structured_query_to_proto(definition: &QueryDefinition) -> FirestoreResult<fs::StructuredQuery> {
    let select = definition.projection().map(|fields| fs::structured_query::Projection {
        fields: fields
            .iter()
            .map(|field| fs::structured_query::FieldReference {
                field_path: field.canonical_string(),
            })
            .collect(),
    });

    let from = vec![fs::structured_query::CollectionSelector {
        collection_id: definition.collection_id().to_string(),
        all_descendants: definition.collection_group().is_some(),
    }];

    let filter = if definition.filters().is_empty() {
        None
    } else {
        Some(filters_to_proto(definition.filters())?)
    };

    let order_by = definition
        .request_order_by()
        .iter()
        .map(|order| fs::structured_query::Order {
            field: Some(fs::structured_query::FieldReference {
                field_path: order.field().canonical_string(),
            }),
            direction: match order.direction().as_str() {
                "DESCENDING" => fs::structured_query::Direction::Descending as i32,
                _ => fs::structured_query::Direction::Ascending as i32,
            },
        })
        .collect();

    Ok(fs::StructuredQuery {
        select,
        from,
        r#where: filter,
        order_by,
        start_at: definition
            .request_start_at()
            .map(|bound| cursor_to_proto(bound, true))
            .transpose()?,
        end_at: definition
            .request_end_at()
            .map(|bound| cursor_to_proto(bound, false))
            .transpose()?,
        offset: 0,
        limit: definition.limit().map(|limit| limit as i32),
        find_nearest: None,
    })
}

/// Several top-level filters are joined with `AND`, exactly like the REST encoder does.
fn filters_to_proto(filters: &[Filter]) -> FirestoreResult<fs::structured_query::Filter> {
    if filters.len() == 1 {
        return filter_to_proto(&filters[0]);
    }

    let nested = filters
        .iter()
        .map(filter_to_proto)
        .collect::<FirestoreResult<Vec<_>>>()?;
    Ok(composite(fs::structured_query::composite_filter::Operator::And, nested))
}

fn filter_to_proto(filter: &Filter) -> FirestoreResult<fs::structured_query::Filter> {
    match filter {
        Filter::Field(field) => field_filter_to_proto(field),
        Filter::Composite { operator, filters } => {
            if filters.len() == 1 {
                return filter_to_proto(&filters[0]);
            }
            let operator = match operator.as_str() {
                "OR" => fs::structured_query::composite_filter::Operator::Or,
                _ => fs::structured_query::composite_filter::Operator::And,
            };
            let nested = filters
                .iter()
                .map(filter_to_proto)
                .collect::<FirestoreResult<Vec<_>>>()?;
            Ok(composite(operator, nested))
        }
    }
}

fn composite(
    operator: fs::structured_query::composite_filter::Operator,
    filters: Vec<fs::structured_query::Filter>,
) -> fs::structured_query::Filter {
    fs::structured_query::Filter {
        filter_type: Some(fs::structured_query::filter::FilterType::CompositeFilter(
            fs::structured_query::CompositeFilter {
                op: operator as i32,
                filters,
            },
        )),
    }
}

fn cursor_to_proto(bound: &Bound, start: bool) -> FirestoreResult<fs::Cursor> {
    Ok(fs::Cursor {
        values: bound
            .values()
            .iter()
            .map(value_to_proto)
            .collect::<FirestoreResult<Vec<_>>>()?,
        before: if start { bound.inclusive() } else { !bound.inclusive() },
    })
}

fn field_filter_to_proto(filter: &FieldFilter) -> FirestoreResult<fs::structured_query::Filter> {
    let operator = match filter.operator().as_str() {
        "LESS_THAN" => fs::structured_query::field_filter::Operator::LessThan,
        "LESS_THAN_OR_EQUAL" => fs::structured_query::field_filter::Operator::LessThanOrEqual,
        "GREATER_THAN" => fs::structured_query::field_filter::Operator::GreaterThan,
        "GREATER_THAN_OR_EQUAL" => fs::structured_query::field_filter::Operator::GreaterThanOrEqual,
        "EQUAL" => fs::structured_query::field_filter::Operator::Equal,
        "NOT_EQUAL" => fs::structured_query::field_filter::Operator::NotEqual,
        "ARRAY_CONTAINS" => fs::structured_query::field_filter::Operator::ArrayContains,
        "ARRAY_CONTAINS_ANY" => fs::structured_query::field_filter::Operator::ArrayContainsAny,
        "IN" => fs::structured_query::field_filter::Operator::In,
        "NOT_IN" => fs::structured_query::field_filter::Operator::NotIn,
        other => return Err(internal_error(format!("unsupported filter operator '{other}'"))),
    };

    Ok(fs::structured_query::Filter {
        filter_type: Some(fs::structured_query::filter::FilterType::FieldFilter(
            fs::structured_query::FieldFilter {
                field: Some(fs::structured_query::FieldReference {
                    field_path: filter.field().canonical_string(),
                }),
                op: operator as i32,
                value: Some(value_to_proto(filter.value())?),
            },
        )),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::query::{FilterOperator, OrderDirection, Query};
    use crate::model::{DatabaseId, FieldPath, ResourcePath};
    use crate::value::BytesValue;

    fn serializer() -> JsonProtoSerializer {
        JsonProtoSerializer::new(DatabaseId::new("demo-project", "(default)"))
    }

    fn round_trip(value: FirestoreValue) -> FirestoreValue {
        let encoded = value_to_proto(&value).expect("encode");
        value_from_proto(&encoded).expect("decode")
    }

    #[test]
    fn every_value_kind_round_trips_through_protobuf() {
        assert_eq!(round_trip(FirestoreValue::null()), FirestoreValue::null());
        assert_eq!(round_trip(FirestoreValue::from_bool(true)), FirestoreValue::from_bool(true));
        assert_eq!(round_trip(FirestoreValue::from_integer(-17)), FirestoreValue::from_integer(-17));
        assert_eq!(round_trip(FirestoreValue::from_double(1.5)), FirestoreValue::from_double(1.5));
        assert_eq!(
            round_trip(FirestoreValue::from_string("hello".to_string())),
            FirestoreValue::from_string("hello".to_string())
        );
        assert_eq!(
            round_trip(FirestoreValue::from_timestamp(Timestamp::new(1_700_000_000, 123))),
            FirestoreValue::from_timestamp(Timestamp::new(1_700_000_000, 123))
        );
        assert_eq!(
            round_trip(FirestoreValue::from_bytes(BytesValue::new(vec![1, 2, 3]))),
            FirestoreValue::from_bytes(BytesValue::new(vec![1, 2, 3]))
        );
        assert_eq!(
            round_trip(FirestoreValue::from_reference(
                "projects/p/databases/(default)/documents/c/d".to_string()
            )),
            FirestoreValue::from_reference("projects/p/databases/(default)/documents/c/d".to_string())
        );
        assert_eq!(
            round_trip(FirestoreValue::from_geo_point(GeoPoint::new(1.0, 2.0).unwrap())),
            FirestoreValue::from_geo_point(GeoPoint::new(1.0, 2.0).unwrap())
        );

        let nested = FirestoreValue::from_array(vec![
            FirestoreValue::from_integer(1),
            FirestoreValue::from_map(
                [("inner".to_string(), FirestoreValue::from_string("value".to_string()))]
                    .into_iter()
                    .collect(),
            ),
        ]);
        assert_eq!(round_trip(nested.clone()), nested);
    }

    #[test]
    fn sentinels_cannot_be_sent_on_a_listen_stream() {
        let err = value_to_proto(&FirestoreValue::server_timestamp()).expect_err("sentinels are write-only");
        assert_eq!(err.code_str(), "firestore/invalid-argument");
    }

    #[test]
    fn documents_decode_into_the_watch_model() {
        let serializer = serializer();
        let document = fs::Document {
            name: "projects/demo-project/databases/(default)/documents/rooms/lobby".to_string(),
            fields: [(
                "seats".to_string(),
                fs::Value {
                    value_type: Some(fs::value::ValueType::IntegerValue(4)),
                },
            )]
            .into_iter()
            .collect(),
            create_time: Some(ProtoTimestamp { seconds: 10, nanos: 0 }),
            update_time: Some(ProtoTimestamp { seconds: 20, nanos: 5 }),
        };

        let decoded = document_from_proto(&serializer, &document).expect("decode");
        assert_eq!(decoded.key.path().canonical_string(), "rooms/lobby");
        assert_eq!(decoded.fields.fields().get("seats"), Some(&FirestoreValue::from_integer(4)));
        assert_eq!(decoded.create_time, Some(Timestamp::new(10, 0)));
        assert_eq!(decoded.update_time, Some(Timestamp::new(20, 5)));
    }

    fn query_for(path: &str) -> Query {
        use firebase_core::app::{FirebaseApp, FirebaseAppConfig, FirebaseOptions};
        let options = FirebaseOptions {
            project_id: Some("demo-project".into()),
            ..Default::default()
        };
        let config = FirebaseAppConfig::new("listen-convert-test", false);
        let container = firebase_core::component::ComponentContainer::new("listen-convert-test");
        let app = FirebaseApp::new(options, config, container);
        let firestore = crate::Firestore::new(app, DatabaseId::new("demo-project", "(default)"));
        Query::new(firestore, ResourcePath::from_string(path).expect("path")).expect("query")
    }

    #[test]
    fn structured_queries_carry_filters_order_and_cursors() {
        let field = |name: &str| FieldPath::from_dot_separated(name).expect("field");
        let query = query_for("rooms")
            .where_field(field("seats"), FilterOperator::GreaterThan, FirestoreValue::from_integer(2))
            .expect("filter")
            .order_by(field("seats"), OrderDirection::Descending)
            .expect("order")
            .limit(5)
            .expect("limit")
            .start_at(vec![FirestoreValue::from_integer(10)])
            .expect("cursor");

        let proto = structured_query_to_proto(&query.definition()).expect("encode");

        assert_eq!(proto.from.len(), 1);
        assert_eq!(proto.from[0].collection_id, "rooms");
        assert!(!proto.from[0].all_descendants);
        assert_eq!(proto.limit, Some(5));

        let filter = proto.r#where.expect("where clause");
        match filter.filter_type.expect("filter type") {
            fs::structured_query::filter::FilterType::FieldFilter(field_filter) => {
                assert_eq!(field_filter.field.expect("field").field_path, "seats");
                assert_eq!(
                    field_filter.op,
                    fs::structured_query::field_filter::Operator::GreaterThan as i32
                );
            }
            other => panic!("expected a field filter, got {other:?}"),
        }

        // `seats` is ordered explicitly and `__name__` is appended, matching the REST encoder.
        let order_fields: Vec<String> = proto
            .order_by
            .iter()
            .map(|order| order.field.as_ref().expect("field").field_path.clone())
            .collect();
        assert_eq!(order_fields, vec!["seats".to_string(), "__name__".to_string()]);
        assert_eq!(proto.order_by[0].direction, fs::structured_query::Direction::Descending as i32);

        let start = proto.start_at.expect("start cursor");
        assert!(start.before, "start_at is inclusive");
        assert_eq!(start.values.len(), 1);
    }

    #[test]
    fn composite_filters_are_encoded_as_nested_filters() {
        let field = |name: &str| FieldPath::from_dot_separated(name).expect("field");
        let query = query_for("rooms")
            .where_filter(crate::api::query::or(vec![
                crate::api::query::where_filter(field("seats"), FilterOperator::Equal, FirestoreValue::from_integer(2)),
                crate::api::query::where_filter(field("seats"), FilterOperator::Equal, FirestoreValue::from_integer(4)),
            ]))
            .expect("composite filter");

        let proto = structured_query_to_proto(&query.definition()).expect("encode");
        match proto.r#where.expect("where").filter_type.expect("type") {
            fs::structured_query::filter::FilterType::CompositeFilter(composite) => {
                assert_eq!(composite.op, fs::structured_query::composite_filter::Operator::Or as i32);
                assert_eq!(composite.filters.len(), 2);
            }
            other => panic!("expected a composite filter, got {other:?}"),
        }
    }
}
