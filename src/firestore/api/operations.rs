use std::collections::{BTreeMap, HashSet};

use crate::firestore::error::{invalid_argument, FirestoreResult};
use crate::firestore::model::{DocumentKey, FieldPath};
use crate::firestore::value::{FirestoreValue, MapValue, SentinelValue, ValueKind};

/// Options that configure the behaviour of `set_doc`/`set_doc_with_converter` writes.
///
/// Mirrors the modular JS `SetOptions` type from
/// `packages/firestore/src/lite-api/reference_impl.ts`, including `merge` and
/// `mergeFields` support.
#[derive(Clone, Debug, Default)]
pub struct SetOptions {
    /// When `true`, `set_doc` merges the provided data into the existing
    /// document, matching the JS `merge: true` option.
    pub merge: bool,
    /// Explicit field mask that should be merged. When set, this takes
    /// precedence over the `merge` flag.
    pub merge_fields: Option<Vec<FieldPath>>,
}

impl SetOptions {
    /// Builds set options that merge every field present in the provided data.
    ///
    /// TypeScript reference: `SetOptions` (`merge: true`) in
    /// `packages/firestore/src/lite-api/reference_impl.ts`.
    pub fn merge_all() -> Self {
        Self {
            merge: true,
            merge_fields: None,
        }
    }

    /// Builds set options that merge only the specified field paths.
    ///
    /// TypeScript reference: `SetOptions` (`mergeFields`) in
    /// `packages/firestore/src/lite-api/reference_impl.ts`.
    pub fn merge_fields<I>(fields: I) -> FirestoreResult<Self>
    where
        I: IntoIterator<Item = FieldPath>,
    {
        let mut unique = Vec::new();
        let mut seen = HashSet::new();
        for field in fields {
            if seen.insert(field.canonical_string()) {
                unique.push(field);
            }
        }
        if unique.is_empty() {
            return Err(invalid_argument("merge_fields requires at least one field path"));
        }
        Ok(Self {
            merge: false,
            merge_fields: Some(unique),
        })
    }

    /// Indicates whether the write should behave like a merge.
    pub fn is_merge(&self) -> bool {
        self.merge || self.merge_fields.is_some()
    }

    /// Returns the explicit field mask, if any.
    pub fn field_mask(&self) -> Option<&[FieldPath]> {
        self.merge_fields.as_deref()
    }
}

/// Pre-encoded data for `set` style writes.
#[derive(Clone, Debug)]
pub struct EncodedSetData {
    pub map: MapValue,
    pub mask: Option<Vec<FieldPath>>,
    pub transforms: Vec<FieldTransform>,
}

/// Pre-encoded data for `update` style writes.
#[derive(Clone, Debug)]
pub struct EncodedUpdateData {
    pub map: MapValue,
    pub field_paths: Vec<FieldPath>,
    pub transforms: Vec<FieldTransform>,
}

/// Describes a single field transform applied during a write.
#[derive(Clone, Debug)]
pub struct FieldTransform {
    field_path: FieldPath,
    operation: TransformOperation,
}

impl FieldTransform {
    pub fn new(field_path: FieldPath, operation: TransformOperation) -> Self {
        Self { field_path, operation }
    }

    pub fn field_path(&self) -> &FieldPath {
        &self.field_path
    }

    pub fn operation(&self) -> &TransformOperation {
        &self.operation
    }
}

/// Write-time sentinel operations supported by Firestore.
#[derive(Clone, Debug)]
pub enum TransformOperation {
    ServerTimestamp,
    ArrayUnion(Vec<FirestoreValue>),
    ArrayRemove(Vec<FirestoreValue>),
    NumericIncrement(FirestoreValue),
}

//#[allow(dead_code)]
pub fn encode_document_data(data: BTreeMap<String, FirestoreValue>) -> FirestoreResult<MapValue> {
    Ok(MapValue::new(data))
}

pub fn encode_set_data(
    data: BTreeMap<String, FirestoreValue>,
    options: &SetOptions,
) -> FirestoreResult<EncodedSetData> {
    let (sanitized, transforms, sentinel_paths, deleted_paths) = sanitize_for_write(data)?;
    if !deleted_paths.is_empty() && !options.is_merge() {
        return Err(invalid_argument(
            "deleteField() can only be used with update() and set() with {merge:true}",
        ));
    }

    let mut available_paths = collect_update_paths(&sanitized)?;
    available_paths.extend(sentinel_paths.iter().cloned());
    available_paths.extend(deleted_paths.iter().cloned());

    let mut available_set = HashSet::new();
    let mut deduped_paths = Vec::new();
    for path in available_paths {
        if available_set.insert(path.canonical_string()) {
            deduped_paths.push(path);
        }
    }

    let mask = if let Some(mask) = options.field_mask() {
        validate_mask_against_available(mask, &available_set)?;
        Some(mask.to_vec())
    } else if options.merge {
        if deduped_paths.is_empty() {
            return Err(invalid_argument("merge set requires the data to contain at least one field"));
        }
        Some(deduped_paths)
    } else {
        None
    };

    let map = MapValue::new(sanitized);
    Ok(EncodedSetData { map, mask, transforms })
}

pub fn encode_update_document_data(data: BTreeMap<String, FirestoreValue>) -> FirestoreResult<EncodedUpdateData> {
    let (sanitized, transforms, _sentinel_paths, deleted_paths) = sanitize_for_write(data)?;
    if sanitized.is_empty() && transforms.is_empty() && deleted_paths.is_empty() {
        return Err(invalid_argument("update_doc requires at least one field/value pair"));
    }
    // Deleted fields appear in the update mask without a value, which is how the backend
    // removes them.
    let mut field_paths = collect_update_paths(&sanitized)?;
    field_paths.extend(deleted_paths);
    let map = MapValue::new(sanitized);
    Ok(EncodedUpdateData {
        map,
        field_paths,
        transforms,
    })
}

//#[allow(dead_code)]
pub fn validate_document_path(path: &str) -> FirestoreResult<DocumentKey> {
    let key = DocumentKey::from_string(path)?;
    Ok(key)
}

type SanitizedWrite = (
    BTreeMap<String, FirestoreValue>,
    Vec<FieldTransform>,
    Vec<FieldPath>,
    Vec<FieldPath>,
);

/// Splits user data into plain fields, field transforms, the paths of those transforms, and the
/// paths marked with `delete_field()`.
fn sanitize_for_write(data: BTreeMap<String, FirestoreValue>) -> FirestoreResult<SanitizedWrite> {
    let mut transforms = Vec::new();
    let mut sentinel_paths = Vec::new();
    let mut deleted_paths = Vec::new();
    let sanitized = sanitize_map(&data, &[], &mut transforms, &mut sentinel_paths, &mut deleted_paths)?;
    Ok((sanitized, transforms, sentinel_paths, deleted_paths))
}

fn sanitize_map(
    data: &BTreeMap<String, FirestoreValue>,
    parent_segments: &[String],
    transforms: &mut Vec<FieldTransform>,
    sentinel_paths: &mut Vec<FieldPath>,
    deleted_paths: &mut Vec<FieldPath>,
) -> FirestoreResult<BTreeMap<String, FirestoreValue>> {
    let mut cleaned = BTreeMap::new();
    for (key, value) in data {
        let mut segments = parent_segments.to_vec();
        segments.push(key.clone());
        let field_path = FieldPath::new(segments.clone())?;
        match value.kind().clone() {
            ValueKind::Sentinel(SentinelValue::DeleteField) => {
                deleted_paths.push(field_path);
            }
            ValueKind::Sentinel(sentinel) => {
                validate_sentinel_usage(&sentinel, &field_path)?;
                transforms.push(transform_from_sentinel(field_path.clone(), sentinel)?);
                sentinel_paths.push(field_path);
            }
            ValueKind::Map(map) => {
                let nested = sanitize_map(map.fields(), &segments, transforms, sentinel_paths, deleted_paths)?;
                if !nested.is_empty() {
                    cleaned.insert(key.clone(), FirestoreValue::from_map(nested));
                }
            }
            ValueKind::Array(_) => {
                assert_no_sentinel_in_value(value, &field_path)?;
                cleaned.insert(key.clone(), value.clone());
            }
            _ => {
                cleaned.insert(key.clone(), value.clone());
            }
        }
    }
    Ok(cleaned)
}

fn validate_sentinel_usage(sentinel: &SentinelValue, field_path: &FieldPath) -> FirestoreResult<()> {
    match sentinel {
        SentinelValue::ServerTimestamp => Ok(()),
        SentinelValue::ArrayUnion(elements) | SentinelValue::ArrayRemove(elements) => {
            for element in elements {
                assert_no_sentinel_in_value(element, field_path)?;
            }
            Ok(())
        }
        SentinelValue::NumericIncrement(operand) => match operand.as_ref().kind() {
            ValueKind::Integer(_) | ValueKind::Double(_) => Ok(()),
            _ => Err(invalid_argument("FieldValue.increment() requires a numeric operand")),
        },
        SentinelValue::DeleteField => Ok(()),
    }
}

fn transform_from_sentinel(field_path: FieldPath, sentinel: SentinelValue) -> FirestoreResult<FieldTransform> {
    let operation = match sentinel {
        SentinelValue::ServerTimestamp => TransformOperation::ServerTimestamp,
        SentinelValue::ArrayUnion(elements) => TransformOperation::ArrayUnion(elements),
        SentinelValue::ArrayRemove(elements) => TransformOperation::ArrayRemove(elements),
        SentinelValue::NumericIncrement(operand) => TransformOperation::NumericIncrement(*operand),
        SentinelValue::DeleteField => {
            return Err(invalid_argument(format!(
                "deleteField() is not a transform (field '{}')",
                field_path.canonical_string()
            )))
        }
    };
    Ok(FieldTransform::new(field_path, operation))
}

fn assert_no_sentinel_in_value(value: &FirestoreValue, context: &FieldPath) -> FirestoreResult<()> {
    match value.kind() {
        ValueKind::Sentinel(_) => Err(invalid_argument(format!(
            "Invalid data. Sentinel values cannot be used inside arrays (field '{}').",
            context.canonical_string()
        ))),
        ValueKind::Array(array) => {
            for element in array.values() {
                assert_no_sentinel_in_value(element, context)?;
            }
            Ok(())
        }
        ValueKind::Map(map) => {
            for element in map.fields().values() {
                assert_no_sentinel_in_value(element, context)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

fn validate_mask_against_available(mask: &[FieldPath], available: &HashSet<String>) -> FirestoreResult<()> {
    for field in mask {
        if !available.contains(field.canonical_string().as_str()) {
            return Err(invalid_argument(format!(
                "Field '{}' is specified in merge_fields but missing from the provided data",
                field.canonical_string()
            )));
        }
    }
    Ok(())
}

fn collect_update_paths(data: &BTreeMap<String, FirestoreValue>) -> FirestoreResult<Vec<FieldPath>> {
    let mut paths = Vec::new();
    for (key, value) in data {
        let mut segments = Vec::new();
        segments.push(key.clone());
        collect_paths_from_value(&mut paths, segments, value)?;
    }
    Ok(paths)
}

fn collect_paths_from_value(
    acc: &mut Vec<FieldPath>,
    segments: Vec<String>,
    value: &FirestoreValue,
) -> FirestoreResult<()> {
    match value.kind() {
        ValueKind::Map(map) if !map.fields().is_empty() => {
            for (child_key, child_value) in map.fields() {
                let mut child_segments = segments.clone();
                child_segments.push(child_key.clone());
                collect_paths_from_value(acc, child_segments, child_value)?;
            }
            Ok(())
        }
        _ => {
            acc.push(FieldPath::new(segments)?);
            Ok(())
        }
    }
}

pub(crate) fn value_for_field_path(map: &MapValue, path: &FieldPath) -> Option<FirestoreValue> {
    map.get(path).cloned()
}

pub(crate) fn set_value_at_field_path(
    fields: &mut BTreeMap<String, FirestoreValue>,
    path: &FieldPath,
    value: FirestoreValue,
) {
    set_value_at_segments(fields, path.segments(), value);
}

fn set_value_at_segments(fields: &mut BTreeMap<String, FirestoreValue>, segments: &[String], value: FirestoreValue) {
    if segments.is_empty() {
        return;
    }

    if segments.len() == 1 {
        fields.insert(segments[0].clone(), value);
        return;
    }

    let first = &segments[0];
    let entry = fields
        .entry(first.clone())
        .or_insert_with(|| FirestoreValue::from_map(BTreeMap::new()));

    let mut child_fields = match entry.kind() {
        ValueKind::Map(map) => map.fields().clone(),
        _ => BTreeMap::new(),
    };

    set_value_at_segments(&mut child_fields, &segments[1..], value);
    *entry = FirestoreValue::from_map(child_fields);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::firestore::model::FieldPath;

    #[test]
    fn merge_collects_sentinel_paths() {
        let mut data = BTreeMap::new();
        data.insert("updated_at".to_string(), FirestoreValue::server_timestamp());
        let options = SetOptions::merge_all();
        let encoded = encode_set_data(data, &options).unwrap();
        let mask = encoded.mask.expect("mask");
        assert_eq!(mask.len(), 1);
        assert_eq!(mask[0].canonical_string(), "updated_at");
        assert_eq!(encoded.transforms.len(), 1);
        matches!(encoded.transforms[0].operation(), TransformOperation::ServerTimestamp);
    }

    #[test]
    fn merge_fields_supports_sentinel() {
        let mut data = BTreeMap::new();
        data.insert(
            "stats".to_string(),
            FirestoreValue::from_map(BTreeMap::from([(
                "last_updated".to_string(),
                FirestoreValue::server_timestamp(),
            )])),
        );
        let options =
            SetOptions::merge_fields(vec![FieldPath::from_dot_separated("stats.last_updated").unwrap()]).unwrap();
        let encoded = encode_set_data(data, &options).unwrap();
        assert_eq!(encoded.mask.unwrap().len(), 1);
        assert_eq!(encoded.transforms.len(), 1);
    }

    #[test]
    fn update_with_only_transform_is_allowed() {
        let mut data = BTreeMap::new();
        data.insert(
            "counter".to_string(),
            FirestoreValue::numeric_increment(FirestoreValue::from_integer(1)),
        );
        let encoded = encode_update_document_data(data).unwrap();
        assert!(encoded.map.fields().is_empty());
        assert!(encoded.field_paths.is_empty());
        assert_eq!(encoded.transforms.len(), 1);
    }

    #[test]
    fn array_rejects_nested_sentinel() {
        let mut data = BTreeMap::new();
        data.insert(
            "values".to_string(),
            FirestoreValue::from_array(vec![FirestoreValue::server_timestamp()]),
        );
        let err = encode_set_data(data, &SetOptions::default()).unwrap_err();
        assert_eq!(err.code_str(), "firestore/invalid-argument");
    }

    #[test]
    fn increment_requires_numeric_operand() {
        let mut data = BTreeMap::new();
        data.insert(
            "total".to_string(),
            FirestoreValue::numeric_increment(FirestoreValue::from_string("five")),
        );
        let err = encode_update_document_data(data).unwrap_err();
        assert_eq!(err.code_str(), "firestore/invalid-argument");
    }
}

/// Removes the value at `path`, if present. Empty parent maps are left in place, matching the
/// backend, which keeps an empty map after its last key is deleted.
pub(crate) fn remove_value_at_field_path(fields: &mut BTreeMap<String, FirestoreValue>, path: &FieldPath) {
    remove_segments(fields, path.segments());
}

fn remove_segments(fields: &mut BTreeMap<String, FirestoreValue>, segments: &[String]) {
    match segments {
        [] => {}
        [last] => {
            fields.remove(last);
        }
        [head, rest @ ..] => {
            if let Some(child) = fields.get_mut(head) {
                if let ValueKind::Map(map) = child.kind() {
                    let mut nested = map.fields().clone();
                    remove_segments(&mut nested, rest);
                    *child = FirestoreValue::from_map(nested);
                }
            }
        }
    }
}

#[cfg(test)]
mod delete_field_tests {
    use super::*;

    fn nested(a_b: FirestoreValue) -> BTreeMap<String, FirestoreValue> {
        let mut inner = BTreeMap::new();
        inner.insert("b".to_string(), a_b);
        let mut data = BTreeMap::new();
        data.insert("a".to_string(), FirestoreValue::from_map(inner));
        data.insert("keep".to_string(), FirestoreValue::from_integer(1));
        data
    }

    #[test]
    fn update_puts_deleted_paths_in_the_mask_without_values() {
        let encoded = encode_update_document_data(nested(FirestoreValue::delete_field())).unwrap();
        let paths: Vec<String> = encoded.field_paths.iter().map(FieldPath::canonical_string).collect();
        assert_eq!(paths, vec!["keep", "a.b"]);
        assert!(
            !encoded.map.fields().contains_key("a"),
            "deleted field must not be sent as a value"
        );
        assert!(encoded.transforms.is_empty());
    }

    #[test]
    fn delete_field_alone_is_a_valid_update() {
        let mut data = BTreeMap::new();
        data.insert("gone".to_string(), FirestoreValue::delete_field());
        let encoded = encode_update_document_data(data).unwrap();
        assert_eq!(encoded.field_paths.len(), 1);
        assert!(encoded.map.fields().is_empty());
    }

    #[test]
    fn delete_field_requires_merge_for_set() {
        let mut data = BTreeMap::new();
        data.insert("gone".to_string(), FirestoreValue::delete_field());
        let err = encode_set_data(data.clone(), &SetOptions::default()).unwrap_err();
        assert!(err.to_string().contains("deleteField()"));
        let merged = encode_set_data(data, &SetOptions::merge_all()).unwrap();
        let mask: Vec<String> = merged.mask.unwrap().iter().map(FieldPath::canonical_string).collect();
        assert_eq!(mask, vec!["gone"]);
    }

    #[test]
    fn remove_value_at_field_path_handles_nesting() {
        let mut fields = nested(FirestoreValue::from_integer(7));
        remove_value_at_field_path(&mut fields, &FieldPath::from_dot_separated("a.b").unwrap());
        match fields["a"].kind() {
            ValueKind::Map(map) => assert!(map.fields().is_empty()),
            other => panic!("unexpected {other:?}"),
        }
        remove_value_at_field_path(&mut fields, &FieldPath::from_dot_separated("missing.x").unwrap());
        remove_value_at_field_path(&mut fields, &FieldPath::from_dot_separated("keep").unwrap());
        assert!(!fields.contains_key("keep"));
    }
}
