use std::collections::BTreeMap;

use crate::firestore::error::FirestoreResult;
use crate::firestore::value::{FirestoreValue, MapValue};

/// Trait describing how to convert between user models and Firestore maps.
///
/// This mirrors the modular JS `FirestoreDataConverter` contract: writes use
/// `to_map`, reads use `from_map`, and callers choose the `Model` type they
/// want to surface.
pub trait FirestoreDataConverter: Send + Sync + Clone + 'static {
    /// The strongly typed model associated with this converter.
    type Model: Clone;

    /// Encodes the user model into a Firestore map for writes.
    fn to_map(&self, value: &Self::Model) -> FirestoreResult<BTreeMap<String, FirestoreValue>>;

    /// Decodes a Firestore map into the user model for reads.
    fn from_map(&self, value: &MapValue) -> FirestoreResult<Self::Model>;
}

/// Default converter that leaves Firestore maps unchanged (raw JSON-style data).
#[derive(Clone, Default)]
pub struct PassthroughConverter;

impl FirestoreDataConverter for PassthroughConverter {
    type Model = BTreeMap<String, FirestoreValue>;

    fn to_map(&self, value: &Self::Model) -> FirestoreResult<BTreeMap<String, FirestoreValue>> {
        Ok(value.clone())
    }

    fn from_map(&self, value: &MapValue) -> FirestoreResult<Self::Model> {
        Ok(value.fields().clone())
    }
}

/// Converter backed by serde: any `Serialize + DeserializeOwned` type becomes a document model.
///
/// ```no_run
/// # use firebase_rs_sdk::firestore::*;
/// # use serde::{Serialize, Deserialize};
/// #[derive(Serialize, Deserialize, Clone)]
/// struct City { name: String, population: i64 }
///
/// # async fn demo(client: FirestoreClient, firestore: Firestore) -> FirestoreResult<()> {
/// let cities = firestore.collection("cities")?.with_converter(SerdeConverter::<City>::new());
/// let snapshot = client.add_doc_with_converter(&cities, City { name: "Lima".into(), population: 10_000_000 }).await?;
/// let city: Option<City> = snapshot.data()?;
/// # Ok(()) }
/// ```
pub struct SerdeConverter<T> {
    _model: std::marker::PhantomData<fn() -> T>,
}

impl<T> SerdeConverter<T> {
    pub fn new() -> Self {
        Self {
            _model: std::marker::PhantomData,
        }
    }
}

impl<T> Default for SerdeConverter<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> Clone for SerdeConverter<T> {
    fn clone(&self) -> Self {
        Self::new()
    }
}

impl<T> FirestoreDataConverter for SerdeConverter<T>
where
    T: serde::Serialize + serde::de::DeserializeOwned + Clone + Send + Sync + 'static,
{
    type Model = T;

    fn to_map(&self, value: &Self::Model) -> FirestoreResult<BTreeMap<String, FirestoreValue>> {
        crate::firestore::value::to_document(value)
    }

    fn from_map(&self, value: &MapValue) -> FirestoreResult<Self::Model> {
        crate::firestore::value::from_document(value.fields())
    }
}
