use crate::firestore::error::{invalid_argument, FirestoreResult};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct FieldPath {
    segments: Vec<String>,
}

impl FieldPath {
    pub fn new<S, I>(segments: I) -> FirestoreResult<Self>
    where
        S: Into<String>,
        I: IntoIterator<Item = S>,
    {
        let segments: Vec<String> = segments.into_iter().map(Into::into).collect();
        if segments.is_empty() {
            return Err(invalid_argument("FieldPath must contain at least one segment"));
        }
        Ok(Self { segments })
    }

    /// Parses a dot-separated path such as `address.city`. Mirrors the JS SDK's string form:
    /// dots always separate segments (use [`FieldPath::new`] for a field name that contains a
    /// dot), empty segments are rejected, and the reserved characters `~ * / [ ]` are refused.
    pub fn from_dot_separated(path: &str) -> FirestoreResult<Self> {
        if path.trim().is_empty() {
            return Err(invalid_argument("FieldPath string cannot be empty"));
        }
        if path.chars().any(|c| matches!(c, '~' | '*' | '/' | '[' | ']')) {
            return Err(invalid_argument(format!(
                "Invalid field path ({path}). Paths must not contain '~', '*', '/', '[', or ']'"
            )));
        }
        if path.split('.').any(str::is_empty) {
            return Err(invalid_argument(format!(
                "Invalid field path ({path}). Paths must not be empty, begin with '.', end with '.', or contain '..'"
            )));
        }
        FieldPath::new(path.split('.'))
    }

    pub fn last_segment(&self) -> &str {
        self.segments
            .last()
            .expect("FieldPath always has at least one segment")
            .as_str()
    }

    pub fn segments(&self) -> &[String] {
        &self.segments
    }

    /// The wire form of the path: segments joined by dots, with any segment that is not a plain
    /// identifier wrapped in backticks (escaping backslashes and backticks), exactly as
    /// `FieldPath.canonicalString()` does in the JS SDK. This is what `updateMask`, filters and
    /// `orderBy` send to the backend, so a field literally named `a.b` addresses that field.
    pub fn canonical_string(&self) -> String {
        self.segments
            .iter()
            .map(|segment| {
                let escaped = segment.replace('\\', "\\\\").replace('`', "\\`");
                if is_simple_identifier(segment) {
                    escaped
                } else {
                    format!("`{escaped}`")
                }
            })
            .collect::<Vec<_>>()
            .join(".")
    }

    pub fn to_vec(&self) -> Vec<String> {
        self.segments.clone()
    }

    pub fn document_id() -> Self {
        Self {
            segments: vec!["__name__".to_string()],
        }
    }
}

fn is_simple_identifier(segment: &str) -> bool {
    let mut chars = segment.chars();
    match chars.next() {
        Some(first) if first == '_' || first.is_ascii_alphabetic() => {}
        _ => return false,
    }
    chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

/// Trait that converts common user inputs into a validated [`FieldPath`].
pub trait IntoFieldPath {
    fn into_field_path(self) -> FirestoreResult<FieldPath>;
}

impl IntoFieldPath for FieldPath {
    fn into_field_path(self) -> FirestoreResult<FieldPath> {
        Ok(self)
    }
}

impl<'a> IntoFieldPath for &'a FieldPath {
    fn into_field_path(self) -> FirestoreResult<FieldPath> {
        Ok(self.clone())
    }
}

impl IntoFieldPath for String {
    fn into_field_path(self) -> FirestoreResult<FieldPath> {
        FieldPath::from_dot_separated(&self)
    }
}

impl<'a> IntoFieldPath for &'a str {
    fn into_field_path(self) -> FirestoreResult<FieldPath> {
        FieldPath::from_dot_separated(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_dot_path() {
        let field = FieldPath::from_dot_separated("foo.bar").unwrap();
        assert_eq!(field.segments(), &["foo", "bar"]);
    }

    #[test]
    fn rejects_empty() {
        let err = FieldPath::from_dot_separated("").unwrap_err();
        assert_eq!(err.code_str(), "firestore/invalid-argument");
    }
}
