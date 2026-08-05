//! JSON to BSON document encoding (issue #8).
//!
//! BSON is the payload format for stored documents. It preserves types that a
//! text encoding would flatten, and it is what issues #1 and #2 both specified.
//!
//! Only JSON objects are storable. BSON's top-level type *is* a document, so a
//! bare scalar or array has no representation; rejecting those here beats
//! silently wrapping them in a synthetic field the caller never asked for.

use serde_json::Value;

use crate::error::{CndbError, Result};

/// Encode a JSON object as BSON bytes.
pub fn to_bson_bytes(value: &Value) -> Result<Vec<u8>> {
    if !value.is_object() {
        return Err(CndbError::NotAnObject {
            found: type_name(value),
        });
    }
    let doc = bson::to_document(value).map_err(|e| CndbError::Bson(e.to_string()))?;
    let mut out = Vec::new();
    doc.to_writer(&mut out)
        .map_err(|e| CndbError::Bson(e.to_string()))?;
    Ok(out)
}

/// Decode BSON bytes back into a JSON object.
pub fn from_bson_bytes(bytes: &[u8]) -> Result<Value> {
    let doc: bson::Document =
        bson::from_slice(bytes).map_err(|e| CndbError::Bson(e.to_string()))?;
    bson::from_document(doc).map_err(|e| CndbError::Bson(e.to_string()))
}

/// The JSON type name, for error messages.
fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn roundtrip(value: Value) -> Value {
        from_bson_bytes(&to_bson_bytes(&value).unwrap()).unwrap()
    }

    #[test]
    fn flat_objects_roundtrip() {
        let doc = json!({ "name": "parse_header", "line": 42, "public": true });
        assert_eq!(roundtrip(doc.clone()), doc);
    }

    #[test]
    fn nested_objects_and_arrays_roundtrip() {
        // Shaped like the graph nodes M2 will store.
        let doc = json!({
            "kind": "Function",
            "qualified_name": "cndb::storage::engine::StorageEngine::commit",
            "span": { "start_line": 231, "end_line": 268, "start_byte": 7104 },
            "params": [
                { "name": "self", "by_ref": true },
                { "name": "force", "by_ref": false }
            ],
            "doc": null,
            "tags": ["storage", "commit", "durability"]
        });
        assert_eq!(roundtrip(doc.clone()), doc);
    }

    #[test]
    fn scalar_types_keep_their_identity() {
        let doc = json!({
            "null": null,
            "true": true,
            "false": false,
            "int": 42,
            "negative": -7,
            "zero": 0,
            "float": 1.5,
            "string": "",
            "unicode": "λ → ∀ 日本語",
            "empty_object": {},
            "empty_array": [],
        });
        let out = roundtrip(doc.clone());
        assert_eq!(out, doc);
        assert!(out["null"].is_null());
        assert_eq!(out["int"], json!(42));
        assert_eq!(out["float"], json!(1.5));
    }

    #[test]
    fn integers_do_not_silently_become_floats() {
        let out = roundtrip(json!({ "n": 9_007_199_254_740_993i64 }));
        assert_eq!(out["n"], json!(9_007_199_254_740_993i64));
        assert!(out["n"].is_i64(), "large integers must stay integral");
    }

    #[test]
    fn deep_nesting_roundtrips() {
        let mut doc = json!({ "leaf": true });
        for i in 0..32 {
            doc = json!({ "depth": i, "child": doc });
        }
        assert_eq!(roundtrip(doc.clone()), doc);
    }

    #[test]
    fn non_objects_are_refused_with_their_type_named() {
        for (value, expected) in [
            (json!(null), "null"),
            (json!(true), "a boolean"),
            (json!(3), "a number"),
            (json!("text"), "a string"),
            (json!([1, 2, 3]), "an array"),
        ] {
            match to_bson_bytes(&value) {
                Err(CndbError::NotAnObject { found }) => assert_eq!(found, expected),
                other => panic!("expected {value} to be refused, got {other:?}"),
            }
        }
    }

    #[test]
    fn garbage_bytes_do_not_decode() {
        assert!(from_bson_bytes(b"not bson at all").is_err());
        assert!(from_bson_bytes(&[]).is_err());
    }

    #[test]
    fn a_truncated_document_does_not_decode() {
        let bytes = to_bson_bytes(&json!({ "key": "a value long enough to cut" })).unwrap();
        assert!(from_bson_bytes(&bytes[..bytes.len() - 5]).is_err());
    }
}
