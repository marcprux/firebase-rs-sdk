use crate::model::{FieldPath, ResourcePath};
use crate::error::FirestoreResult;

pub fn resource_path(segments: &[&str]) -> ResourcePath {
    ResourcePath::from_segments(segments.iter().cloned())
}

pub fn field_path(segments: &[&str]) -> FirestoreResult<FieldPath> {
    FieldPath::new(segments.iter().cloned())
}

pub fn split_resource_path(path: &str) -> ResourcePath {
    ResourcePath::from_segments(path.split('/'))
}

