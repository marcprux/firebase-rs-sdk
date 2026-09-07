//! Serde integration: convert any `Serialize` type into a [`FirestoreValue`] tree and any
//! `Deserialize` type back out of one.
//!
//! The JS SDK accepts plain objects; the Rust equivalent is `#[derive(Serialize, Deserialize)]`
//! structs. Firestore-specific types keep their native representation:
//!
//! | Rust type | Firestore value |
//! |---|---|
//! | `bool`, integers, floats, `String`, `char` | boolean / integer / double / string |
//! | `Option<T>` (`None`), `()` | null |
//! | `Vec<T>`, tuples | array |
//! | maps with string keys, structs | map |
//! | unit enum variants | string; other variants become `{ "Variant": payload }` |
//! | [`Timestamp`] | timestamp |
//! | [`GeoPoint`] | geo point |
//! | [`BytesValue`] | bytes (a plain `Vec<u8>` becomes an array of integers) |
//! | [`FirestoreValue`] | itself, including sentinels such as `server_timestamp()` |
//!
//! Sentinel values can be written but never read back.

use std::collections::BTreeMap;
use std::fmt::Display;

use serde::de::{self, DeserializeOwned, DeserializeSeed, IntoDeserializer, MapAccess, SeqAccess, Visitor};
use serde::ser::{self, SerializeMap, SerializeSeq, SerializeStruct, SerializeTuple, SerializeTupleStruct};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::firestore::error::{invalid_argument, FirestoreError, FirestoreResult};
use crate::firestore::model::{GeoPoint, Timestamp};
use crate::firestore::value::{BytesValue, FirestoreValue, SentinelValue, ValueKind};

// Marker names understood by the serializer/deserializer below. They never leave this crate;
// other serde formats simply see ordinary structs with these (unusual) names.
const TIMESTAMP_MARKER: &str = "$__firestore_timestamp";
const GEO_POINT_MARKER: &str = "$__firestore_geo_point";
const BYTES_MARKER: &str = "$__firestore_bytes";
const REFERENCE_MARKER: &str = "$__firestore_reference";
const VALUE_MARKER: &str = "$__firestore_value";
const SERVER_TIMESTAMP_MARKER: &str = "$__firestore_server_timestamp";
const INCREMENT_MARKER: &str = "$__firestore_increment";
const ARRAY_UNION_MARKER: &str = "$__firestore_array_union";
const ARRAY_REMOVE_MARKER: &str = "$__firestore_array_remove";
const KIND_KEY: &str = "$__firestore_kind";

impl ser::Error for FirestoreError {
    fn custom<T: Display>(msg: T) -> Self {
        invalid_argument(format!("serialization error: {msg}"))
    }
}

impl de::Error for FirestoreError {
    fn custom<T: Display>(msg: T) -> Self {
        invalid_argument(format!("deserialization error: {msg}"))
    }
}

/// Serializes `value` into a [`FirestoreValue`].
pub fn to_firestore_value<T: Serialize + ?Sized>(value: &T) -> FirestoreResult<FirestoreValue> {
    value.serialize(ValueSerializer)
}

/// Serializes `value` into the field map of a document. The value must serialize as a map or
/// struct.
pub fn to_document<T: Serialize + ?Sized>(value: &T) -> FirestoreResult<BTreeMap<String, FirestoreValue>> {
    match to_firestore_value(value)?.into_kind() {
        ValueKind::Map(map) => Ok(map.into_fields()),
        other => Err(invalid_argument(format!(
            "a Firestore document must serialize to a map, got {}",
            kind_name(&other)
        ))),
    }
}

/// Deserializes a `T` from a [`FirestoreValue`].
pub fn from_firestore_value<T: DeserializeOwned>(value: &FirestoreValue) -> FirestoreResult<T> {
    T::deserialize(ValueDeserializer { value })
}

/// Deserializes a `T` from the field map of a document.
pub fn from_document<T: DeserializeOwned>(fields: &BTreeMap<String, FirestoreValue>) -> FirestoreResult<T> {
    T::deserialize(MapDeserializer::new(fields))
}

fn kind_name(kind: &ValueKind) -> &'static str {
    match kind {
        ValueKind::Null => "null",
        ValueKind::Boolean(_) => "boolean",
        ValueKind::Integer(_) => "integer",
        ValueKind::Double(_) => "double",
        ValueKind::Timestamp(_) => "timestamp",
        ValueKind::String(_) => "string",
        ValueKind::Bytes(_) => "bytes",
        ValueKind::Reference(_) => "reference",
        ValueKind::GeoPoint(_) => "geo point",
        ValueKind::Array(_) => "array",
        ValueKind::Map(_) => "map",
        ValueKind::Sentinel(_) => "sentinel",
    }
}

// ----------------------------------------------------------------------------------------------
// Serialize impls for Firestore types
// ----------------------------------------------------------------------------------------------

impl Serialize for Timestamp {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct(TIMESTAMP_MARKER, 2)?;
        state.serialize_field("seconds", &self.seconds)?;
        state.serialize_field("nanos", &self.nanos)?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for Timestamp {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Raw {
            seconds: i64,
            #[serde(default)]
            nanos: i32,
        }
        let raw = Raw::deserialize(deserializer)?;
        Ok(Timestamp::new(raw.seconds, raw.nanos))
    }
}

impl Serialize for GeoPoint {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct(GEO_POINT_MARKER, 2)?;
        state.serialize_field("latitude", &self.latitude())?;
        state.serialize_field("longitude", &self.longitude())?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for GeoPoint {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Raw {
            latitude: f64,
            longitude: f64,
        }
        let raw = Raw::deserialize(deserializer)?;
        GeoPoint::new(raw.latitude, raw.longitude).map_err(de::Error::custom)
    }
}

impl Serialize for BytesValue {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        struct Raw<'a>(&'a [u8]);
        impl Serialize for Raw<'_> {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.serialize_bytes(self.0)
            }
        }
        serializer.serialize_newtype_struct(BYTES_MARKER, &Raw(self.as_slice()))
    }
}

impl<'de> Deserialize<'de> for BytesValue {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct BytesVisitor;
        impl<'de> Visitor<'de> for BytesVisitor {
            type Value = BytesValue;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("bytes")
            }
            fn visit_bytes<E: de::Error>(self, v: &[u8]) -> Result<Self::Value, E> {
                Ok(BytesValue::new(v.to_vec()))
            }
            fn visit_byte_buf<E: de::Error>(self, v: Vec<u8>) -> Result<Self::Value, E> {
                Ok(BytesValue::new(v))
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
                BytesValue::from_base64(v).map_err(E::custom)
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                let mut out = Vec::new();
                while let Some(byte) = seq.next_element::<u8>()? {
                    out.push(byte);
                }
                Ok(BytesValue::new(out))
            }
        }
        deserializer.deserialize_byte_buf(BytesVisitor)
    }
}

impl Serialize for FirestoreValue {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self.kind() {
            ValueKind::Null => serializer.serialize_none(),
            ValueKind::Boolean(v) => serializer.serialize_bool(*v),
            ValueKind::Integer(v) => serializer.serialize_i64(*v),
            ValueKind::Double(v) => serializer.serialize_f64(*v),
            ValueKind::Timestamp(v) => v.serialize(serializer),
            ValueKind::String(v) => serializer.serialize_str(v),
            ValueKind::Bytes(v) => v.serialize(serializer),
            ValueKind::Reference(path) => serializer.serialize_newtype_struct(REFERENCE_MARKER, path),
            ValueKind::GeoPoint(v) => v.serialize(serializer),
            ValueKind::Array(array) => {
                let mut seq = serializer.serialize_seq(Some(array.values().len()))?;
                for value in array.values() {
                    seq.serialize_element(value)?;
                }
                seq.end()
            }
            ValueKind::Map(map) => {
                let mut state = serializer.serialize_map(Some(map.fields().len()))?;
                for (key, value) in map.fields() {
                    state.serialize_entry(key, value)?;
                }
                state.end()
            }
            ValueKind::Sentinel(sentinel) => match sentinel {
                SentinelValue::ServerTimestamp => serializer.serialize_unit_struct(SERVER_TIMESTAMP_MARKER),
                SentinelValue::NumericIncrement(operand) => {
                    serializer.serialize_newtype_struct(INCREMENT_MARKER, operand.as_ref())
                }
                SentinelValue::ArrayUnion(elements) => {
                    serializer.serialize_newtype_struct(ARRAY_UNION_MARKER, elements)
                }
                SentinelValue::ArrayRemove(elements) => {
                    serializer.serialize_newtype_struct(ARRAY_REMOVE_MARKER, elements)
                }
            },
        }
    }
}

impl<'de> Deserialize<'de> for FirestoreValue {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_newtype_struct(VALUE_MARKER, ValueVisitor)
    }
}

/// Builds a `FirestoreValue` from whatever a deserializer offers. When the source is
/// [`ValueDeserializer`] it receives a tagged map for the Firestore-specific kinds; any other
/// source yields plain JSON-like data.
struct ValueVisitor;

impl<'de> Visitor<'de> for ValueVisitor {
    type Value = FirestoreValue;

    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("any Firestore value")
    }

    fn visit_bool<E: de::Error>(self, v: bool) -> Result<Self::Value, E> {
        Ok(FirestoreValue::from_bool(v))
    }
    fn visit_i64<E: de::Error>(self, v: i64) -> Result<Self::Value, E> {
        Ok(FirestoreValue::from_integer(v))
    }
    fn visit_u64<E: de::Error>(self, v: u64) -> Result<Self::Value, E> {
        i64::try_from(v)
            .map(FirestoreValue::from_integer)
            .map_err(|_| E::custom("integer does not fit in an i64"))
    }
    fn visit_f64<E: de::Error>(self, v: f64) -> Result<Self::Value, E> {
        Ok(FirestoreValue::from_double(v))
    }
    fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
        Ok(FirestoreValue::from_string(v))
    }
    fn visit_string<E: de::Error>(self, v: String) -> Result<Self::Value, E> {
        Ok(FirestoreValue::from_string(v))
    }
    fn visit_bytes<E: de::Error>(self, v: &[u8]) -> Result<Self::Value, E> {
        Ok(FirestoreValue::from_bytes(BytesValue::new(v.to_vec())))
    }
    fn visit_byte_buf<E: de::Error>(self, v: Vec<u8>) -> Result<Self::Value, E> {
        Ok(FirestoreValue::from_bytes(BytesValue::new(v)))
    }
    fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
        Ok(FirestoreValue::null())
    }
    fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
        Ok(FirestoreValue::null())
    }
    fn visit_some<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_any(self)
    }
    fn visit_newtype_struct<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_any(self)
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        let mut values = Vec::new();
        while let Some(value) = seq.next_element::<FirestoreValue>()? {
            values.push(value);
        }
        Ok(FirestoreValue::from_array(values))
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut fields = BTreeMap::new();
        let mut kind: Option<String> = None;
        while let Some(key) = map.next_key::<String>()? {
            if key == KIND_KEY {
                kind = Some(map.next_value::<String>()?);
            } else {
                fields.insert(key, map.next_value::<FirestoreValue>()?);
            }
        }
        let Some(kind) = kind else {
            return Ok(FirestoreValue::from_map(fields));
        };
        let field = |name: &str| fields.get(name).cloned();
        match kind.as_str() {
            "timestamp" => {
                let seconds = field("seconds").and_then(|v| integer_of(&v));
                let nanos = field("nanos").and_then(|v| integer_of(&v)).unwrap_or(0);
                match seconds {
                    Some(seconds) => Ok(FirestoreValue::from_timestamp(Timestamp::new(seconds, nanos as i32))),
                    None => Err(de::Error::custom("timestamp marker without seconds")),
                }
            }
            "geo_point" => {
                let latitude = field("latitude").and_then(|v| double_of(&v));
                let longitude = field("longitude").and_then(|v| double_of(&v));
                match (latitude, longitude) {
                    (Some(latitude), Some(longitude)) => GeoPoint::new(latitude, longitude)
                        .map(FirestoreValue::from_geo_point)
                        .map_err(de::Error::custom),
                    _ => Err(de::Error::custom("geo point marker without coordinates")),
                }
            }
            "reference" => match field("path").map(|v| v.into_kind()) {
                Some(ValueKind::String(path)) => Ok(FirestoreValue::from_reference(path)),
                _ => Err(de::Error::custom("reference marker without path")),
            },
            "bytes" => match field("data").map(|v| v.into_kind()) {
                Some(ValueKind::Bytes(bytes)) => Ok(FirestoreValue::from_bytes(bytes)),
                Some(ValueKind::String(base64)) => BytesValue::from_base64(&base64)
                    .map(FirestoreValue::from_bytes)
                    .map_err(de::Error::custom),
                _ => Err(de::Error::custom("bytes marker without data")),
            },
            other => Err(de::Error::custom(format!("unknown Firestore value marker {other}"))),
        }
    }
}

fn integer_of(value: &FirestoreValue) -> Option<i64> {
    match value.kind() {
        ValueKind::Integer(i) => Some(*i),
        ValueKind::Double(d) => Some(*d as i64),
        _ => None,
    }
}

fn double_of(value: &FirestoreValue) -> Option<f64> {
    match value.kind() {
        ValueKind::Integer(i) => Some(*i as f64),
        ValueKind::Double(d) => Some(*d),
        _ => None,
    }
}

// ----------------------------------------------------------------------------------------------
// Serializer
// ----------------------------------------------------------------------------------------------

/// A `serde::Serializer` that produces [`FirestoreValue`]s.
pub struct ValueSerializer;

impl Serializer for ValueSerializer {
    type Ok = FirestoreValue;
    type Error = FirestoreError;
    type SerializeSeq = SeqState;
    type SerializeTuple = SeqState;
    type SerializeTupleStruct = SeqState;
    type SerializeTupleVariant = VariantSeqState;
    type SerializeMap = MapState;
    type SerializeStruct = MapState;
    type SerializeStructVariant = VariantMapState;

    fn serialize_bool(self, v: bool) -> FirestoreResult<FirestoreValue> {
        Ok(FirestoreValue::from_bool(v))
    }
    fn serialize_i8(self, v: i8) -> FirestoreResult<FirestoreValue> {
        Ok(FirestoreValue::from_integer(v as i64))
    }
    fn serialize_i16(self, v: i16) -> FirestoreResult<FirestoreValue> {
        Ok(FirestoreValue::from_integer(v as i64))
    }
    fn serialize_i32(self, v: i32) -> FirestoreResult<FirestoreValue> {
        Ok(FirestoreValue::from_integer(v as i64))
    }
    fn serialize_i64(self, v: i64) -> FirestoreResult<FirestoreValue> {
        Ok(FirestoreValue::from_integer(v))
    }
    fn serialize_u8(self, v: u8) -> FirestoreResult<FirestoreValue> {
        Ok(FirestoreValue::from_integer(v as i64))
    }
    fn serialize_u16(self, v: u16) -> FirestoreResult<FirestoreValue> {
        Ok(FirestoreValue::from_integer(v as i64))
    }
    fn serialize_u32(self, v: u32) -> FirestoreResult<FirestoreValue> {
        Ok(FirestoreValue::from_integer(v as i64))
    }
    fn serialize_u64(self, v: u64) -> FirestoreResult<FirestoreValue> {
        i64::try_from(v)
            .map(FirestoreValue::from_integer)
            .map_err(|_| invalid_argument(format!("integer {v} does not fit in Firestore's 64-bit signed integer")))
    }
    fn serialize_f32(self, v: f32) -> FirestoreResult<FirestoreValue> {
        Ok(FirestoreValue::from_double(v as f64))
    }
    fn serialize_f64(self, v: f64) -> FirestoreResult<FirestoreValue> {
        Ok(FirestoreValue::from_double(v))
    }
    fn serialize_char(self, v: char) -> FirestoreResult<FirestoreValue> {
        Ok(FirestoreValue::from_string(v.to_string()))
    }
    fn serialize_str(self, v: &str) -> FirestoreResult<FirestoreValue> {
        Ok(FirestoreValue::from_string(v))
    }
    fn serialize_bytes(self, v: &[u8]) -> FirestoreResult<FirestoreValue> {
        Ok(FirestoreValue::from_bytes(BytesValue::new(v.to_vec())))
    }
    fn serialize_none(self) -> FirestoreResult<FirestoreValue> {
        Ok(FirestoreValue::null())
    }
    fn serialize_some<T: Serialize + ?Sized>(self, value: &T) -> FirestoreResult<FirestoreValue> {
        value.serialize(self)
    }
    fn serialize_unit(self) -> FirestoreResult<FirestoreValue> {
        Ok(FirestoreValue::null())
    }
    fn serialize_unit_struct(self, name: &'static str) -> FirestoreResult<FirestoreValue> {
        if name == SERVER_TIMESTAMP_MARKER {
            Ok(FirestoreValue::server_timestamp())
        } else {
            Ok(FirestoreValue::null())
        }
    }
    fn serialize_unit_variant(
        self,
        _name: &'static str,
        _index: u32,
        variant: &'static str,
    ) -> FirestoreResult<FirestoreValue> {
        Ok(FirestoreValue::from_string(variant))
    }
    fn serialize_newtype_struct<T: Serialize + ?Sized>(
        self,
        name: &'static str,
        value: &T,
    ) -> FirestoreResult<FirestoreValue> {
        match name {
            REFERENCE_MARKER => match value.serialize(ValueSerializer)?.into_kind() {
                ValueKind::String(path) => Ok(FirestoreValue::from_reference(path)),
                other => Err(invalid_argument(format!(
                    "reference marker expects a string path, got {}",
                    kind_name(&other)
                ))),
            },
            INCREMENT_MARKER => {
                let operand = value.serialize(ValueSerializer)?;
                match operand.kind() {
                    ValueKind::Integer(_) | ValueKind::Double(_) => Ok(FirestoreValue::numeric_increment(operand)),
                    other => Err(invalid_argument(format!(
                        "increment expects a number, got {}",
                        kind_name(other)
                    ))),
                }
            }
            ARRAY_UNION_MARKER | ARRAY_REMOVE_MARKER => match value.serialize(ValueSerializer)?.into_kind() {
                ValueKind::Array(array) => {
                    let elements = array.into_values();
                    Ok(if name == ARRAY_UNION_MARKER {
                        FirestoreValue::array_union(elements)
                    } else {
                        FirestoreValue::array_remove(elements)
                    })
                }
                other => Err(invalid_argument(format!(
                    "array transform expects an array, got {}",
                    kind_name(&other)
                ))),
            },
            // BYTES_MARKER, VALUE_MARKER and user newtypes are transparent.
            _ => value.serialize(self),
        }
    }
    fn serialize_newtype_variant<T: Serialize + ?Sized>(
        self,
        _name: &'static str,
        _index: u32,
        variant: &'static str,
        value: &T,
    ) -> FirestoreResult<FirestoreValue> {
        let mut fields = BTreeMap::new();
        fields.insert(variant.to_string(), value.serialize(ValueSerializer)?);
        Ok(FirestoreValue::from_map(fields))
    }
    fn serialize_seq(self, len: Option<usize>) -> FirestoreResult<SeqState> {
        Ok(SeqState {
            values: Vec::with_capacity(len.unwrap_or(0)),
        })
    }
    fn serialize_tuple(self, len: usize) -> FirestoreResult<SeqState> {
        self.serialize_seq(Some(len))
    }
    fn serialize_tuple_struct(self, _name: &'static str, len: usize) -> FirestoreResult<SeqState> {
        self.serialize_seq(Some(len))
    }
    fn serialize_tuple_variant(
        self,
        _name: &'static str,
        _index: u32,
        variant: &'static str,
        len: usize,
    ) -> FirestoreResult<VariantSeqState> {
        Ok(VariantSeqState {
            variant,
            values: Vec::with_capacity(len),
        })
    }
    fn serialize_map(self, _len: Option<usize>) -> FirestoreResult<MapState> {
        Ok(MapState {
            marker: None,
            fields: BTreeMap::new(),
            pending_key: None,
        })
    }
    fn serialize_struct(self, name: &'static str, _len: usize) -> FirestoreResult<MapState> {
        Ok(MapState {
            marker: match name {
                TIMESTAMP_MARKER | GEO_POINT_MARKER => Some(name),
                _ => None,
            },
            fields: BTreeMap::new(),
            pending_key: None,
        })
    }
    fn serialize_struct_variant(
        self,
        _name: &'static str,
        _index: u32,
        variant: &'static str,
        _len: usize,
    ) -> FirestoreResult<VariantMapState> {
        Ok(VariantMapState {
            variant,
            fields: BTreeMap::new(),
        })
    }
}

/// Serializes map keys; only strings (and chars) are allowed.
struct KeySerializer;

impl Serializer for KeySerializer {
    type Ok = String;
    type Error = FirestoreError;
    type SerializeSeq = ser::Impossible<String, FirestoreError>;
    type SerializeTuple = ser::Impossible<String, FirestoreError>;
    type SerializeTupleStruct = ser::Impossible<String, FirestoreError>;
    type SerializeTupleVariant = ser::Impossible<String, FirestoreError>;
    type SerializeMap = ser::Impossible<String, FirestoreError>;
    type SerializeStruct = ser::Impossible<String, FirestoreError>;
    type SerializeStructVariant = ser::Impossible<String, FirestoreError>;

    fn serialize_str(self, v: &str) -> FirestoreResult<String> {
        Ok(v.to_string())
    }
    fn serialize_char(self, v: char) -> FirestoreResult<String> {
        Ok(v.to_string())
    }
    fn serialize_unit_variant(
        self,
        _name: &'static str,
        _index: u32,
        variant: &'static str,
    ) -> FirestoreResult<String> {
        Ok(variant.to_string())
    }
    fn serialize_newtype_struct<T: Serialize + ?Sized>(
        self,
        _name: &'static str,
        value: &T,
    ) -> FirestoreResult<String> {
        value.serialize(self)
    }

    fn serialize_bool(self, _: bool) -> FirestoreResult<String> {
        Err(key_error("bool"))
    }
    fn serialize_i8(self, v: i8) -> FirestoreResult<String> {
        Ok(v.to_string())
    }
    fn serialize_i16(self, v: i16) -> FirestoreResult<String> {
        Ok(v.to_string())
    }
    fn serialize_i32(self, v: i32) -> FirestoreResult<String> {
        Ok(v.to_string())
    }
    fn serialize_i64(self, v: i64) -> FirestoreResult<String> {
        Ok(v.to_string())
    }
    fn serialize_u8(self, v: u8) -> FirestoreResult<String> {
        Ok(v.to_string())
    }
    fn serialize_u16(self, v: u16) -> FirestoreResult<String> {
        Ok(v.to_string())
    }
    fn serialize_u32(self, v: u32) -> FirestoreResult<String> {
        Ok(v.to_string())
    }
    fn serialize_u64(self, v: u64) -> FirestoreResult<String> {
        Ok(v.to_string())
    }
    fn serialize_f32(self, _: f32) -> FirestoreResult<String> {
        Err(key_error("float"))
    }
    fn serialize_f64(self, _: f64) -> FirestoreResult<String> {
        Err(key_error("float"))
    }
    fn serialize_bytes(self, _: &[u8]) -> FirestoreResult<String> {
        Err(key_error("bytes"))
    }
    fn serialize_none(self) -> FirestoreResult<String> {
        Err(key_error("none"))
    }
    fn serialize_some<T: Serialize + ?Sized>(self, value: &T) -> FirestoreResult<String> {
        value.serialize(self)
    }
    fn serialize_unit(self) -> FirestoreResult<String> {
        Err(key_error("unit"))
    }
    fn serialize_unit_struct(self, _: &'static str) -> FirestoreResult<String> {
        Err(key_error("unit struct"))
    }
    fn serialize_newtype_variant<T: Serialize + ?Sized>(
        self,
        _: &'static str,
        _: u32,
        _: &'static str,
        _: &T,
    ) -> FirestoreResult<String> {
        Err(key_error("newtype variant"))
    }
    fn serialize_seq(self, _: Option<usize>) -> FirestoreResult<Self::SerializeSeq> {
        Err(key_error("sequence"))
    }
    fn serialize_tuple(self, _: usize) -> FirestoreResult<Self::SerializeTuple> {
        Err(key_error("tuple"))
    }
    fn serialize_tuple_struct(self, _: &'static str, _: usize) -> FirestoreResult<Self::SerializeTupleStruct> {
        Err(key_error("tuple struct"))
    }
    fn serialize_tuple_variant(
        self,
        _: &'static str,
        _: u32,
        _: &'static str,
        _: usize,
    ) -> FirestoreResult<Self::SerializeTupleVariant> {
        Err(key_error("tuple variant"))
    }
    fn serialize_map(self, _: Option<usize>) -> FirestoreResult<Self::SerializeMap> {
        Err(key_error("map"))
    }
    fn serialize_struct(self, _: &'static str, _: usize) -> FirestoreResult<Self::SerializeStruct> {
        Err(key_error("struct"))
    }
    fn serialize_struct_variant(
        self,
        _: &'static str,
        _: u32,
        _: &'static str,
        _: usize,
    ) -> FirestoreResult<Self::SerializeStructVariant> {
        Err(key_error("struct variant"))
    }
}

fn key_error(what: &str) -> FirestoreError {
    invalid_argument(format!("Firestore map keys must be strings, got {what}"))
}

pub struct SeqState {
    values: Vec<FirestoreValue>,
}

impl SerializeSeq for SeqState {
    type Ok = FirestoreValue;
    type Error = FirestoreError;
    fn serialize_element<T: Serialize + ?Sized>(&mut self, value: &T) -> FirestoreResult<()> {
        self.values.push(value.serialize(ValueSerializer)?);
        Ok(())
    }
    fn end(self) -> FirestoreResult<FirestoreValue> {
        Ok(FirestoreValue::from_array(self.values))
    }
}

impl SerializeTuple for SeqState {
    type Ok = FirestoreValue;
    type Error = FirestoreError;
    fn serialize_element<T: Serialize + ?Sized>(&mut self, value: &T) -> FirestoreResult<()> {
        SerializeSeq::serialize_element(self, value)
    }
    fn end(self) -> FirestoreResult<FirestoreValue> {
        SerializeSeq::end(self)
    }
}

impl SerializeTupleStruct for SeqState {
    type Ok = FirestoreValue;
    type Error = FirestoreError;
    fn serialize_field<T: Serialize + ?Sized>(&mut self, value: &T) -> FirestoreResult<()> {
        SerializeSeq::serialize_element(self, value)
    }
    fn end(self) -> FirestoreResult<FirestoreValue> {
        SerializeSeq::end(self)
    }
}

pub struct VariantSeqState {
    variant: &'static str,
    values: Vec<FirestoreValue>,
}

impl ser::SerializeTupleVariant for VariantSeqState {
    type Ok = FirestoreValue;
    type Error = FirestoreError;
    fn serialize_field<T: Serialize + ?Sized>(&mut self, value: &T) -> FirestoreResult<()> {
        self.values.push(value.serialize(ValueSerializer)?);
        Ok(())
    }
    fn end(self) -> FirestoreResult<FirestoreValue> {
        let mut fields = BTreeMap::new();
        fields.insert(self.variant.to_string(), FirestoreValue::from_array(self.values));
        Ok(FirestoreValue::from_map(fields))
    }
}

pub struct MapState {
    marker: Option<&'static str>,
    fields: BTreeMap<String, FirestoreValue>,
    pending_key: Option<String>,
}

impl MapState {
    fn finish(self) -> FirestoreResult<FirestoreValue> {
        match self.marker {
            Some(TIMESTAMP_MARKER) => {
                let seconds = self.fields.get("seconds").and_then(integer_of).unwrap_or(0);
                let nanos = self.fields.get("nanos").and_then(integer_of).unwrap_or(0);
                Ok(FirestoreValue::from_timestamp(Timestamp::new(seconds, nanos as i32)))
            }
            Some(GEO_POINT_MARKER) => {
                let latitude = self.fields.get("latitude").and_then(double_of).unwrap_or(0.0);
                let longitude = self.fields.get("longitude").and_then(double_of).unwrap_or(0.0);
                GeoPoint::new(latitude, longitude).map(FirestoreValue::from_geo_point)
            }
            _ => Ok(FirestoreValue::from_map(self.fields)),
        }
    }
}

impl SerializeMap for MapState {
    type Ok = FirestoreValue;
    type Error = FirestoreError;
    fn serialize_key<T: Serialize + ?Sized>(&mut self, key: &T) -> FirestoreResult<()> {
        self.pending_key = Some(key.serialize(KeySerializer)?);
        Ok(())
    }
    fn serialize_value<T: Serialize + ?Sized>(&mut self, value: &T) -> FirestoreResult<()> {
        let key = self
            .pending_key
            .take()
            .ok_or_else(|| invalid_argument("serialize_value called before serialize_key"))?;
        self.fields.insert(key, value.serialize(ValueSerializer)?);
        Ok(())
    }
    fn end(self) -> FirestoreResult<FirestoreValue> {
        self.finish()
    }
}

impl SerializeStruct for MapState {
    type Ok = FirestoreValue;
    type Error = FirestoreError;
    fn serialize_field<T: Serialize + ?Sized>(&mut self, key: &'static str, value: &T) -> FirestoreResult<()> {
        self.fields.insert(key.to_string(), value.serialize(ValueSerializer)?);
        Ok(())
    }
    fn end(self) -> FirestoreResult<FirestoreValue> {
        self.finish()
    }
}

pub struct VariantMapState {
    variant: &'static str,
    fields: BTreeMap<String, FirestoreValue>,
}

impl ser::SerializeStructVariant for VariantMapState {
    type Ok = FirestoreValue;
    type Error = FirestoreError;
    fn serialize_field<T: Serialize + ?Sized>(&mut self, key: &'static str, value: &T) -> FirestoreResult<()> {
        self.fields.insert(key.to_string(), value.serialize(ValueSerializer)?);
        Ok(())
    }
    fn end(self) -> FirestoreResult<FirestoreValue> {
        let mut outer = BTreeMap::new();
        outer.insert(self.variant.to_string(), FirestoreValue::from_map(self.fields));
        Ok(FirestoreValue::from_map(outer))
    }
}

// ----------------------------------------------------------------------------------------------
// Deserializer
// ----------------------------------------------------------------------------------------------

/// A `serde::Deserializer` reading from a borrowed [`FirestoreValue`].
pub struct ValueDeserializer<'a> {
    value: &'a FirestoreValue,
}

impl<'a> ValueDeserializer<'a> {
    pub fn new(value: &'a FirestoreValue) -> Self {
        Self { value }
    }

    fn tagged_map<'de, V: Visitor<'de>>(
        &self,
        kind: &str,
        entries: Vec<(&str, FirestoreValue)>,
        visitor: V,
    ) -> FirestoreResult<V::Value> {
        let mut fields: Vec<(String, FirestoreValue)> = vec![(KIND_KEY.to_string(), FirestoreValue::from_string(kind))];
        fields.extend(entries.into_iter().map(|(k, v)| (k.to_string(), v)));
        visitor.visit_map(OwnedMapAccess::new(fields))
    }
}

impl<'de, 'a> Deserializer<'de> for ValueDeserializer<'a> {
    type Error = FirestoreError;

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> FirestoreResult<V::Value> {
        match self.value.kind() {
            ValueKind::Null => visitor.visit_none(),
            ValueKind::Boolean(v) => visitor.visit_bool(*v),
            ValueKind::Integer(v) => visitor.visit_i64(*v),
            ValueKind::Double(v) => visitor.visit_f64(*v),
            ValueKind::String(v) => visitor.visit_str(v),
            ValueKind::Bytes(v) => visitor.visit_bytes(v.as_slice()),
            ValueKind::Array(array) => visitor.visit_seq(SeqAccessImpl {
                iter: array.values().iter(),
            }),
            ValueKind::Map(map) => visitor.visit_map(MapDeserializer::new(map.fields())),
            ValueKind::Timestamp(t) => visitor.visit_map(MapDeserializerOwned::new(vec![
                ("seconds".to_string(), FirestoreValue::from_integer(t.seconds)),
                ("nanos".to_string(), FirestoreValue::from_integer(t.nanos as i64)),
            ])),
            ValueKind::GeoPoint(g) => visitor.visit_map(MapDeserializerOwned::new(vec![
                ("latitude".to_string(), FirestoreValue::from_double(g.latitude())),
                ("longitude".to_string(), FirestoreValue::from_double(g.longitude())),
            ])),
            ValueKind::Reference(path) => visitor.visit_str(path),
            ValueKind::Sentinel(_) => Err(invalid_argument("sentinel values cannot be deserialized")),
        }
    }

    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> FirestoreResult<V::Value> {
        match self.value.kind() {
            ValueKind::Null => visitor.visit_none(),
            _ => visitor.visit_some(self),
        }
    }

    fn deserialize_newtype_struct<V: Visitor<'de>>(self, name: &'static str, visitor: V) -> FirestoreResult<V::Value> {
        if name == VALUE_MARKER {
            // Hand the visitor a tagged representation for the Firestore-specific kinds so that a
            // `FirestoreValue` round-trips losslessly.
            return match self.value.kind() {
                ValueKind::Timestamp(t) => self.tagged_map(
                    "timestamp",
                    vec![
                        ("seconds", FirestoreValue::from_integer(t.seconds)),
                        ("nanos", FirestoreValue::from_integer(t.nanos as i64)),
                    ],
                    visitor,
                ),
                ValueKind::GeoPoint(g) => self.tagged_map(
                    "geo_point",
                    vec![
                        ("latitude", FirestoreValue::from_double(g.latitude())),
                        ("longitude", FirestoreValue::from_double(g.longitude())),
                    ],
                    visitor,
                ),
                ValueKind::Reference(path) => {
                    self.tagged_map("reference", vec![("path", FirestoreValue::from_string(path))], visitor)
                }
                // Base64 text rather than a nested bytes value: a nested `FirestoreValue` would
                // re-enter this same tagging path and never terminate.
                ValueKind::Bytes(bytes) => {
                    self.tagged_map("bytes", vec![("data", FirestoreValue::from_string(bytes.to_base64()))], visitor)
                }
                _ => self.deserialize_any(visitor),
            };
        }
        visitor.visit_newtype_struct(self)
    }

    fn deserialize_bytes<V: Visitor<'de>>(self, visitor: V) -> FirestoreResult<V::Value> {
        match self.value.kind() {
            ValueKind::Bytes(bytes) => visitor.visit_bytes(bytes.as_slice()),
            _ => self.deserialize_any(visitor),
        }
    }

    fn deserialize_byte_buf<V: Visitor<'de>>(self, visitor: V) -> FirestoreResult<V::Value> {
        match self.value.kind() {
            ValueKind::Bytes(bytes) => visitor.visit_byte_buf(bytes.as_slice().to_vec()),
            _ => self.deserialize_any(visitor),
        }
    }

    fn deserialize_enum<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _variants: &'static [&'static str],
        visitor: V,
    ) -> FirestoreResult<V::Value> {
        match self.value.kind() {
            ValueKind::String(variant) => visitor.visit_enum(variant.clone().into_deserializer()),
            ValueKind::Map(map) if map.fields().len() == 1 => {
                let (variant, content) = map.fields().iter().next().expect("one entry");
                visitor.visit_enum(EnumAccessImpl {
                    variant: variant.clone(),
                    content,
                })
            }
            other => Err(invalid_argument(format!(
                "expected a string or single-entry map for an enum, got {}",
                kind_name(other)
            ))),
        }
    }

    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string
        unit unit_struct seq tuple tuple_struct map struct identifier ignored_any
    }
}

struct SeqAccessImpl<'a> {
    iter: std::slice::Iter<'a, FirestoreValue>,
}

impl<'de, 'a> SeqAccess<'de> for SeqAccessImpl<'a> {
    type Error = FirestoreError;
    fn next_element_seed<T: DeserializeSeed<'de>>(&mut self, seed: T) -> FirestoreResult<Option<T::Value>> {
        match self.iter.next() {
            Some(value) => seed.deserialize(ValueDeserializer { value }).map(Some),
            None => Ok(None),
        }
    }
    fn size_hint(&self) -> Option<usize> {
        Some(self.iter.len())
    }
}

/// Map access over borrowed document fields; also serves as the top-level deserializer for
/// [`from_document`].
pub struct MapDeserializer<'a> {
    iter: std::collections::btree_map::Iter<'a, String, FirestoreValue>,
    pending: Option<&'a FirestoreValue>,
    len: usize,
}

impl<'a> MapDeserializer<'a> {
    fn new(fields: &'a BTreeMap<String, FirestoreValue>) -> Self {
        Self {
            iter: fields.iter(),
            pending: None,
            len: fields.len(),
        }
    }
}

impl<'de, 'a> MapAccess<'de> for MapDeserializer<'a> {
    type Error = FirestoreError;
    fn next_key_seed<K: DeserializeSeed<'de>>(&mut self, seed: K) -> FirestoreResult<Option<K::Value>> {
        match self.iter.next() {
            Some((key, value)) => {
                self.pending = Some(value);
                seed.deserialize(key.as_str().into_deserializer()).map(Some)
            }
            None => Ok(None),
        }
    }
    fn next_value_seed<V: DeserializeSeed<'de>>(&mut self, seed: V) -> FirestoreResult<V::Value> {
        let value = self
            .pending
            .take()
            .ok_or_else(|| invalid_argument("next_value called before next_key"))?;
        seed.deserialize(ValueDeserializer { value })
    }
    fn size_hint(&self) -> Option<usize> {
        Some(self.len)
    }
}

impl<'de, 'a> Deserializer<'de> for MapDeserializer<'a> {
    type Error = FirestoreError;
    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> FirestoreResult<V::Value> {
        visitor.visit_map(self)
    }
    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string bytes byte_buf option
        unit unit_struct newtype_struct seq tuple tuple_struct map struct enum identifier ignored_any
    }
}

/// Map access over owned entries (used for synthesized timestamp / geo point maps).
struct MapDeserializerOwned {
    entries: std::vec::IntoIter<(String, FirestoreValue)>,
    pending: Option<FirestoreValue>,
}

impl MapDeserializerOwned {
    fn new(entries: Vec<(String, FirestoreValue)>) -> Self {
        Self {
            entries: entries.into_iter(),
            pending: None,
        }
    }
}

type OwnedMapAccess = MapDeserializerOwned;

impl<'de> MapAccess<'de> for MapDeserializerOwned {
    type Error = FirestoreError;
    fn next_key_seed<K: DeserializeSeed<'de>>(&mut self, seed: K) -> FirestoreResult<Option<K::Value>> {
        match self.entries.next() {
            Some((key, value)) => {
                self.pending = Some(value);
                seed.deserialize(key.into_deserializer()).map(Some)
            }
            None => Ok(None),
        }
    }
    fn next_value_seed<V: DeserializeSeed<'de>>(&mut self, seed: V) -> FirestoreResult<V::Value> {
        let value = self
            .pending
            .take()
            .ok_or_else(|| invalid_argument("next_value called before next_key"))?;
        seed.deserialize(ValueDeserializer { value: &value })
    }
}

struct EnumAccessImpl<'a> {
    variant: String,
    content: &'a FirestoreValue,
}

impl<'de, 'a> de::EnumAccess<'de> for EnumAccessImpl<'a> {
    type Error = FirestoreError;
    type Variant = VariantAccessImpl<'a>;
    fn variant_seed<V: DeserializeSeed<'de>>(self, seed: V) -> FirestoreResult<(V::Value, Self::Variant)> {
        let variant = seed.deserialize(self.variant.into_deserializer())?;
        Ok((variant, VariantAccessImpl { content: self.content }))
    }
}

struct VariantAccessImpl<'a> {
    content: &'a FirestoreValue,
}

impl<'de, 'a> de::VariantAccess<'de> for VariantAccessImpl<'a> {
    type Error = FirestoreError;
    fn unit_variant(self) -> FirestoreResult<()> {
        Ok(())
    }
    fn newtype_variant_seed<T: DeserializeSeed<'de>>(self, seed: T) -> FirestoreResult<T::Value> {
        seed.deserialize(ValueDeserializer { value: self.content })
    }
    fn tuple_variant<V: Visitor<'de>>(self, _len: usize, visitor: V) -> FirestoreResult<V::Value> {
        ValueDeserializer { value: self.content }.deserialize_any(visitor)
    }
    fn struct_variant<V: Visitor<'de>>(
        self,
        _fields: &'static [&'static str],
        visitor: V,
    ) -> FirestoreResult<V::Value> {
        ValueDeserializer { value: self.content }.deserialize_any(visitor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::firestore::error::FirestoreErrorCode;
    use serde::{Deserialize, Serialize};
    use std::collections::HashMap;

    #[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
    enum Status {
        Active,
        Suspended { reason: String },
        Retired(u32),
    }

    #[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
    struct Address {
        street: String,
        zip: Option<String>,
    }

    #[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
    struct City {
        name: String,
        population: i64,
        area_km2: f64,
        capital: bool,
        tags: Vec<String>,
        address: Address,
        nickname: Option<String>,
        founded: Timestamp,
        location: GeoPoint,
        logo: BytesValue,
        status: Status,
        extra: HashMap<String, i32>,
        raw: FirestoreValue,
    }

    fn sample() -> City {
        City {
            name: "Amsterdam".into(),
            population: 921_402,
            area_km2: 219.3,
            capital: true,
            tags: vec!["canals".into(), "bikes".into()],
            address: Address {
                street: "Dam 1".into(),
                zip: None,
            },
            nickname: Some("Mokum".into()),
            founded: Timestamp::new(-21_366_115_200, 500),
            location: GeoPoint::new(52.37, 4.9).unwrap(),
            logo: BytesValue::new(vec![0xde, 0xad, 0xbe, 0xef]),
            status: Status::Suspended {
                reason: "flooding".into(),
            },
            extra: HashMap::from([("bridges".to_string(), 1281)]),
            raw: FirestoreValue::from_reference("projects/p/databases/(default)/documents/countries/NL"),
        }
    }

    #[test]
    fn struct_round_trips_through_firestore_values() {
        let city = sample();
        let fields = to_document(&city).expect("serialize");
        assert!(matches!(fields["founded"].kind(), ValueKind::Timestamp(t) if t.nanos == 500));
        assert!(matches!(fields["location"].kind(), ValueKind::GeoPoint(_)));
        assert!(matches!(fields["logo"].kind(), ValueKind::Bytes(_)));
        assert!(matches!(fields["raw"].kind(), ValueKind::Reference(_)));
        assert!(matches!(fields["nickname"].kind(), ValueKind::String(_)));
        assert!(
            matches!(fields["address"].kind(), ValueKind::Map(m) if matches!(m.fields()["zip"].kind(), ValueKind::Null))
        );
        assert!(matches!(fields["status"].kind(), ValueKind::Map(_)));

        let back: City = from_document(&fields).expect("deserialize");
        assert_eq!(back, city);
    }

    #[test]
    fn enum_representations() {
        assert_eq!(
            to_firestore_value(&Status::Active).unwrap(),
            FirestoreValue::from_string("Active")
        );
        let retired = to_firestore_value(&Status::Retired(3)).unwrap();
        assert!(matches!(retired.kind(), ValueKind::Map(m) if m.fields().contains_key("Retired")));
        assert_eq!(from_firestore_value::<Status>(&retired).unwrap(), Status::Retired(3));
        assert_eq!(
            from_firestore_value::<Status>(&FirestoreValue::from_string("Active")).unwrap(),
            Status::Active
        );
    }

    #[test]
    fn sentinels_serialize_but_do_not_deserialize() {
        #[derive(Serialize)]
        struct Update {
            updated_at: FirestoreValue,
            visits: FirestoreValue,
            tags: FirestoreValue,
        }
        let fields = to_document(&Update {
            updated_at: FirestoreValue::server_timestamp(),
            visits: FirestoreValue::numeric_increment(FirestoreValue::from_integer(1)),
            tags: FirestoreValue::array_union(vec![FirestoreValue::from_string("new")]),
        })
        .unwrap();
        assert!(matches!(
            fields["updated_at"].kind(),
            ValueKind::Sentinel(SentinelValue::ServerTimestamp)
        ));
        assert!(matches!(
            fields["visits"].kind(),
            ValueKind::Sentinel(SentinelValue::NumericIncrement(_))
        ));
        assert!(matches!(fields["tags"].kind(), ValueKind::Sentinel(SentinelValue::ArrayUnion(v)) if v.len() == 1));

        let err = from_firestore_value::<i64>(&FirestoreValue::server_timestamp()).unwrap_err();
        assert_eq!(err.code, FirestoreErrorCode::InvalidArgument);
    }

    #[test]
    fn primitives_and_limits() {
        assert_eq!(to_firestore_value(&7u8).unwrap(), FirestoreValue::from_integer(7));
        assert_eq!(to_firestore_value(&-7i32).unwrap(), FirestoreValue::from_integer(-7));
        assert_eq!(to_firestore_value(&1.5f32).unwrap(), FirestoreValue::from_double(1.5));
        assert_eq!(to_firestore_value(&'x').unwrap(), FirestoreValue::from_string("x"));
        assert_eq!(to_firestore_value(&()).unwrap(), FirestoreValue::null());
        assert_eq!(to_firestore_value(&None::<i32>).unwrap(), FirestoreValue::null());
        assert_eq!(
            to_firestore_value(&(1, "a")).unwrap(),
            FirestoreValue::from_array(vec![FirestoreValue::from_integer(1), FirestoreValue::from_string("a"),])
        );
        let err = to_firestore_value(&u64::MAX).unwrap_err();
        assert_eq!(err.code, FirestoreErrorCode::InvalidArgument);
        let err = to_document(&42).unwrap_err();
        assert!(err.to_string().contains("must serialize to a map"));
        let err = to_firestore_value(&HashMap::from([(1u8, "x")])).unwrap();
        assert!(matches!(err.kind(), ValueKind::Map(m) if m.fields().contains_key("1")));
        let err = to_firestore_value(&BTreeMap::from([(true, "x")])).unwrap_err();
        assert!(err.to_string().contains("keys must be strings"));
    }

    #[test]
    fn deserializes_flexible_numbers_and_missing_optionals() {
        #[derive(Deserialize, Debug, PartialEq)]
        struct Loose {
            count: f64,
            ratio: f32,
            small: u8,
            note: Option<String>,
            #[serde(default)]
            missing: Option<i32>,
        }
        let mut fields = BTreeMap::new();
        fields.insert("count".into(), FirestoreValue::from_integer(3));
        fields.insert("ratio".into(), FirestoreValue::from_double(0.5));
        fields.insert("small".into(), FirestoreValue::from_integer(200));
        fields.insert("note".into(), FirestoreValue::null());
        let loose: Loose = from_document(&fields).unwrap();
        assert_eq!(
            loose,
            Loose {
                count: 3.0,
                ratio: 0.5,
                small: 200,
                note: None,
                missing: None
            }
        );

        fields.insert("small".into(), FirestoreValue::from_integer(300));
        let err = from_document::<Loose>(&fields).unwrap_err();
        assert_eq!(err.code, FirestoreErrorCode::InvalidArgument);
    }

    #[test]
    fn firestore_value_field_round_trips_every_kind() {
        let kinds = vec![
            FirestoreValue::null(),
            FirestoreValue::from_bool(true),
            FirestoreValue::from_integer(-1),
            FirestoreValue::from_double(2.5),
            FirestoreValue::from_string("s"),
            FirestoreValue::from_timestamp(Timestamp::new(1, 2)),
            FirestoreValue::from_geo_point(GeoPoint::new(1.0, 2.0).unwrap()),
            FirestoreValue::from_bytes(BytesValue::new(vec![1, 2])),
            FirestoreValue::from_reference("a/b"),
            FirestoreValue::from_array(vec![FirestoreValue::from_integer(1), FirestoreValue::from_string("x")]),
            FirestoreValue::from_map(BTreeMap::from([("k".to_string(), FirestoreValue::from_bool(false))])),
        ];
        for value in kinds {
            let encoded = to_firestore_value(&value).unwrap();
            assert_eq!(encoded, value, "serialize is the identity");
            let decoded: FirestoreValue = from_firestore_value(&value).unwrap();
            assert_eq!(decoded, value, "deserialize is the identity");
        }
    }

    #[test]
    fn timestamp_and_geo_point_work_with_other_formats_too() {
        let json = serde_json::to_string(&Timestamp::new(5, 6)).unwrap();
        assert_eq!(json, r#"{"seconds":5,"nanos":6}"#);
        let back: Timestamp = serde_json::from_str(&json).unwrap();
        assert_eq!(back, Timestamp::new(5, 6));
        let point: GeoPoint = serde_json::from_str(r#"{"latitude":1.5,"longitude":-2.0}"#).unwrap();
        assert_eq!(point.latitude(), 1.5);
        let bytes: BytesValue = serde_json::from_str("[1,2,3]").unwrap();
        assert_eq!(bytes.as_slice(), &[1, 2, 3]);
    }
}
