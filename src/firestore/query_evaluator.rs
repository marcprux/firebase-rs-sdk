use std::cmp::Ordering;

use crate::firestore::api::snapshot::DocumentSnapshot;
use crate::firestore::model::FieldPath;
use crate::firestore::value::{FirestoreValue, MapValue, ValueKind};
use crate::firestore::{
    Bound, CompositeOperator, FieldFilter, Filter, FilterOperator, LimitType, OrderBy, OrderDirection, QueryDefinition,
};

/// Applies the provided query definition to a set of candidate documents and returns
/// the filtered, ordered, and bounded result set.
///
/// Mirrors the behaviour of the Firestore JS query evaluation helpers found in
/// `packages/firestore/src/core/query.ts` and related files by reusing the same
/// ordering, cursor, and limit semantics.
pub(crate) fn apply_query_to_documents(
    documents: Vec<DocumentSnapshot>,
    definition: &QueryDefinition,
) -> Vec<DocumentSnapshot> {
    let mut filtered: Vec<DocumentSnapshot> = documents
        .into_iter()
        .filter(|snapshot| snapshot.exists())
        .filter(|snapshot| document_satisfies_filters(snapshot, definition.filters()))
        .collect();

    filtered.sort_by(|left, right| compare_snapshots(left, right, definition.result_order_by()));

    if let Some(bound) = definition.result_start_at() {
        filtered.retain(|snapshot| !is_before_start_bound(snapshot, bound, definition.result_order_by()));
    }

    if let Some(bound) = definition.result_end_at() {
        filtered.retain(|snapshot| !is_after_end_bound(snapshot, bound, definition.result_order_by()));
    }

    if let Some(limit) = definition.limit() {
        let limit = limit as usize;
        match definition.limit_type() {
            LimitType::First => {
                if filtered.len() > limit {
                    filtered.truncate(limit);
                }
            }
            LimitType::Last => {
                if filtered.len() > limit {
                    let start = filtered.len() - limit;
                    filtered.drain(0..start);
                }
            }
        }
    }

    filtered
}

fn document_satisfies_filters(snapshot: &DocumentSnapshot, filters: &[Filter]) -> bool {
    filters.iter().all(|filter| document_satisfies(snapshot, filter))
}

fn document_satisfies(snapshot: &DocumentSnapshot, filter: &Filter) -> bool {
    match filter {
        Filter::Field(filter) => match get_field_value(snapshot, filter.field()) {
            Some(value) => evaluate_filter(filter, &value),
            None => match filter.operator() {
                FilterOperator::NotEqual => evaluate_filter(filter, &FirestoreValue::null()),
                _ => false,
            },
        },
        Filter::Composite {
            operator: CompositeOperator::And,
            filters,
        } => filters.iter().all(|f| document_satisfies(snapshot, f)),
        Filter::Composite {
            operator: CompositeOperator::Or,
            filters,
        } => filters.iter().any(|f| document_satisfies(snapshot, f)),
    }
}

fn evaluate_filter(filter: &FieldFilter, value: &FirestoreValue) -> bool {
    match filter.operator() {
        FilterOperator::Equal => value == filter.value(),
        FilterOperator::NotEqual => value != filter.value(),
        FilterOperator::LessThan => compare_values(value, filter.value()) == Some(Ordering::Less),
        FilterOperator::LessThanOrEqual => {
            matches!(compare_values(value, filter.value()), Some(Ordering::Less | Ordering::Equal))
        }
        FilterOperator::GreaterThan => compare_values(value, filter.value()) == Some(Ordering::Greater),
        FilterOperator::GreaterThanOrEqual => {
            matches!(compare_values(value, filter.value()), Some(Ordering::Greater | Ordering::Equal))
        }
        FilterOperator::ArrayContains => match value.kind() {
            ValueKind::Array(array) => array_contains(array, filter.value()),
            _ => false,
        },
        FilterOperator::ArrayContainsAny => match (value.kind(), filter.value().kind()) {
            (ValueKind::Array(array), ValueKind::Array(needles)) => array_contains_any(array, needles),
            _ => false,
        },
        FilterOperator::In => match filter.value().kind() {
            ValueKind::Array(values) => values.values().iter().any(|needle| needle == value),
            _ => false,
        },
        FilterOperator::NotIn => match filter.value().kind() {
            ValueKind::Array(values) => {
                !matches!(value.kind(), ValueKind::Null) && values.values().iter().all(|needle| needle != value)
            }
            _ => false,
        },
    }
}

fn get_field_value(snapshot: &DocumentSnapshot, field: &FieldPath) -> Option<FirestoreValue> {
    if field == &FieldPath::document_id() {
        let key = snapshot.document_key();
        return Some(FirestoreValue::from_string(key.path().canonical_string()));
    }

    let map = snapshot.map_value()?;
    find_in_map(map, field.segments()).cloned()
}

fn find_in_map<'a>(map: &'a MapValue, segments: &'a [String]) -> Option<&'a FirestoreValue> {
    let (first, rest) = segments.split_first()?;
    let value = map.fields().get(first)?;
    if rest.is_empty() {
        Some(value)
    } else if let ValueKind::Map(child) = value.kind() {
        find_in_map(child, rest)
    } else {
        None
    }
}

pub(crate) fn compare_snapshots(left: &DocumentSnapshot, right: &DocumentSnapshot, order_by: &[OrderBy]) -> Ordering {
    for order in order_by {
        let left_value = get_field_value(left, order.field()).unwrap_or_else(FirestoreValue::null);
        let right_value = get_field_value(right, order.field()).unwrap_or_else(FirestoreValue::null);

        let mut ordering = compare_values(&left_value, &right_value).unwrap_or(Ordering::Equal);
        if order.direction() == OrderDirection::Descending {
            ordering = ordering.reverse();
        }
        if ordering != Ordering::Equal {
            return ordering;
        }
    }
    Ordering::Equal
}

/// Position of a value kind in Firestore's cross-type ordering
/// (null < boolean < number < timestamp < string < bytes < reference < geo point < array < map).
fn type_order(kind: &ValueKind) -> u8 {
    match kind {
        ValueKind::Null => 0,
        ValueKind::Boolean(_) => 1,
        ValueKind::Integer(_) | ValueKind::Double(_) => 2,
        ValueKind::Timestamp(_) => 3,
        ValueKind::String(_) => 4,
        ValueKind::Bytes(_) => 5,
        ValueKind::Reference(_) => 6,
        ValueKind::GeoPoint(_) => 7,
        ValueKind::Array(_) => 8,
        ValueKind::Map(_) => 9,
        ValueKind::Sentinel(_) => 10,
    }
}

fn compare_numbers(a: f64, b: f64) -> Ordering {
    // NaN sorts before every other number, and equal to itself, as on the backend.
    match (a.is_nan(), b.is_nan()) {
        (true, true) => Ordering::Equal,
        (true, false) => Ordering::Less,
        (false, true) => Ordering::Greater,
        (false, false) => a.partial_cmp(&b).unwrap_or(Ordering::Equal),
    }
}

/// Total ordering over values following the backend's rules, so that local sorting and cursor
/// evaluation agree with what a query would return.
fn compare_values(left: &FirestoreValue, right: &FirestoreValue) -> Option<Ordering> {
    let (l, r) = (left.kind(), right.kind());
    let by_type = type_order(l).cmp(&type_order(r));
    if by_type != Ordering::Equal {
        return Some(by_type);
    }
    Some(match (l, r) {
        (ValueKind::Null, ValueKind::Null) => Ordering::Equal,
        (ValueKind::Boolean(a), ValueKind::Boolean(b)) => a.cmp(b),
        (ValueKind::Integer(a), ValueKind::Integer(b)) => a.cmp(b),
        (ValueKind::Double(a), ValueKind::Double(b)) => compare_numbers(*a, *b),
        (ValueKind::Integer(a), ValueKind::Double(b)) => compare_numbers(*a as f64, *b),
        (ValueKind::Double(a), ValueKind::Integer(b)) => compare_numbers(*a, *b as f64),
        (ValueKind::Timestamp(a), ValueKind::Timestamp(b)) => (a.seconds, a.nanos).cmp(&(b.seconds, b.nanos)),
        (ValueKind::String(a), ValueKind::String(b)) => a.cmp(b),
        (ValueKind::Bytes(a), ValueKind::Bytes(b)) => a.as_slice().cmp(b.as_slice()),
        (ValueKind::Reference(a), ValueKind::Reference(b)) => {
            // References compare segment by segment, like resource paths.
            a.split('/').cmp(b.split('/'))
        }
        (ValueKind::GeoPoint(a), ValueKind::GeoPoint(b)) => {
            compare_numbers(a.latitude(), b.latitude()).then_with(|| compare_numbers(a.longitude(), b.longitude()))
        }
        (ValueKind::Array(a), ValueKind::Array(b)) => {
            for (x, y) in a.values().iter().zip(b.values()) {
                let ordering = compare_values(x, y).unwrap_or(Ordering::Equal);
                if ordering != Ordering::Equal {
                    return Some(ordering);
                }
            }
            a.values().len().cmp(&b.values().len())
        }
        (ValueKind::Map(a), ValueKind::Map(b)) => {
            // Maps compare by key, then value, in key order.
            for ((ka, va), (kb, vb)) in a.fields().iter().zip(b.fields()) {
                let by_key = ka.cmp(kb);
                if by_key != Ordering::Equal {
                    return Some(by_key);
                }
                let by_value = compare_values(va, vb).unwrap_or(Ordering::Equal);
                if by_value != Ordering::Equal {
                    return Some(by_value);
                }
            }
            a.fields().len().cmp(&b.fields().len())
        }
        _ => Ordering::Equal,
    })
}

fn array_contains(array: &crate::firestore::value::ArrayValue, needle: &FirestoreValue) -> bool {
    array.values().iter().any(|candidate| candidate == needle)
}

fn array_contains_any(
    array: &crate::firestore::value::ArrayValue,
    needles: &crate::firestore::value::ArrayValue,
) -> bool {
    needles.values().iter().any(|needle| array_contains(array, needle))
}

fn is_before_start_bound(snapshot: &DocumentSnapshot, bound: &Bound, order_by: &[OrderBy]) -> bool {
    let ordering = compare_snapshot_to_bound(snapshot, bound, order_by);
    if bound.inclusive() {
        ordering == Ordering::Less
    } else {
        ordering != Ordering::Greater
    }
}

fn is_after_end_bound(snapshot: &DocumentSnapshot, bound: &Bound, order_by: &[OrderBy]) -> bool {
    let ordering = compare_snapshot_to_bound(snapshot, bound, order_by);
    if bound.inclusive() {
        ordering == Ordering::Greater
    } else {
        ordering != Ordering::Less
    }
}

fn compare_snapshot_to_bound(snapshot: &DocumentSnapshot, bound: &Bound, order_by: &[OrderBy]) -> Ordering {
    for (index, order) in order_by.iter().enumerate() {
        if index >= bound.values().len() {
            break;
        }

        let bound_value = &bound.values()[index];
        let snapshot_value = get_field_value(snapshot, order.field()).unwrap_or_else(FirestoreValue::null);

        let mut ordering = compare_values(&snapshot_value, bound_value).unwrap_or(Ordering::Equal);
        if order.direction() == OrderDirection::Descending {
            ordering = ordering.reverse();
        }

        if ordering != Ordering::Equal {
            return ordering;
        }
    }
    Ordering::Equal
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::firestore::api::{database::Firestore, query::Query, snapshot::SnapshotMetadata};
    use crate::firestore::model::{DatabaseId, DocumentKey, FieldPath, ResourcePath};
    use crate::firestore::value::{FirestoreValue, MapValue};
    use crate::firestore::OrderDirection;
    use crate::test_support::firebase::test_firebase_app_with_api_key;
    use std::collections::BTreeMap;

    fn build_query() -> Query {
        let app = test_firebase_app_with_api_key("query-evaluator");
        let firestore = Firestore::new(app, DatabaseId::new("test", "(default)"));
        let path = ResourcePath::from_string("cities").unwrap();
        Query::new(firestore, path).unwrap()
    }

    fn snapshot_for(id: &str, population: i64) -> DocumentSnapshot {
        let key = DocumentKey::from_string(&format!("cities/{id}")).unwrap();
        let mut map = BTreeMap::new();
        map.insert("population".into(), FirestoreValue::from_integer(population));
        let metadata = SnapshotMetadata::new(false, false);
        DocumentSnapshot::new(key, Some(MapValue::new(map)), metadata)
    }

    #[test]
    fn applies_limit_and_ordering() {
        let query = build_query()
            .order_by(FieldPath::from_dot_separated("population").unwrap(), OrderDirection::Ascending)
            .unwrap()
            .limit(2)
            .unwrap();
        let definition = query.definition();

        let docs = vec![snapshot_for("sf", 100), snapshot_for("nyc", 50), snapshot_for("la", 75)];

        let result = apply_query_to_documents(docs, &definition);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].id(), "nyc");
        assert_eq!(result[1].id(), "la");
    }
}
