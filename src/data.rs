//! The runtime data model: the item-based currency that flows between nodes.
//!
//! Data on a connection is an **array of [`Item`]s**, not a single value — the
//! model common to mature workflow tools. A node maps its logic over its input
//! items and returns output items.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One element of the data array flowing on a connection between nodes.
///
/// # Examples
/// ```
/// use serde_json::json;
/// use tinyflows::data::Item;
///
/// let item = Item::new(json!({ "email": "a@b.com" })).paired_with(0);
/// assert_eq!(item.json["email"], "a@b.com");
/// assert_eq!(item.paired_item, Some(0));
/// ```
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Item {
    /// The item's primary JSON payload.
    pub json: Value,
    /// Optional binary attachments (e.g. keyed blobs); omitted when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binary: Option<Value>,
    /// Index of the input item that produced this one — best-effort pairing that
    /// lets a later node correlate an output back to its originating input.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paired_item: Option<usize>,
}

impl Item {
    /// Builds an item from a JSON payload, with no binary attachment or pairing.
    #[must_use]
    pub fn new(json: Value) -> Self {
        Self {
            json,
            binary: None,
            paired_item: None,
        }
    }

    /// Records the index of the input item this output item derived from.
    #[must_use]
    pub fn paired_with(mut self, input_index: usize) -> Self {
        self.paired_item = Some(input_index);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn item_round_trips_and_omits_empty_fields() {
        let item = Item::new(json!({ "x": 1 }));
        let s = serde_json::to_string(&item).expect("serialize");
        // `binary` and `paired_item` are omitted from the wire form when unset.
        assert_eq!(s, r#"{"json":{"x":1}}"#);
        let back: Item = serde_json::from_str(&s).expect("deserialize");
        assert_eq!(item, back);
    }

    #[test]
    fn pairing_is_recorded() {
        let item = Item::new(Value::Null).paired_with(3);
        assert_eq!(item.paired_item, Some(3));
    }

    #[test]
    fn new_leaves_binary_and_pairing_unset() {
        let item = Item::new(json!({ "x": 1 }));
        assert_eq!(item.json, json!({ "x": 1 }));
        assert_eq!(item.binary, None);
        assert_eq!(item.paired_item, None);
    }

    #[test]
    fn default_item_is_all_empty() {
        let item = Item::default();
        assert_eq!(item.json, Value::Null);
        assert_eq!(item.binary, None);
        assert_eq!(item.paired_item, None);
        // `Default` matches `new(Null)`.
        assert_eq!(item, Item::new(Value::Null));
    }

    #[test]
    fn binary_attachment_round_trips() {
        let mut item = Item::new(json!({ "name": "report.pdf" }));
        item.binary = Some(json!({ "data": "aGVsbG8=", "mime": "application/pdf" }));

        let s = serde_json::to_string(&item).expect("serialize");
        // With a binary present, the field is emitted.
        assert!(s.contains("\"binary\""));
        let back: Item = serde_json::from_str(&s).expect("deserialize");
        assert_eq!(item, back);
        assert_eq!(
            back.binary,
            Some(json!({ "data": "aGVsbG8=", "mime": "application/pdf" }))
        );
    }

    #[test]
    fn paired_item_serde_round_trips() {
        let item = Item::new(json!({ "x": 1 })).paired_with(7);
        let s = serde_json::to_string(&item).expect("serialize");
        assert!(s.contains("\"paired_item\":7"));
        let back: Item = serde_json::from_str(&s).expect("deserialize");
        assert_eq!(back.paired_item, Some(7));
        assert_eq!(item, back);
    }

    #[test]
    fn value_conversions_round_trip() {
        let item = Item::new(json!({ "k": "v" })).paired_with(2);

        // to_value then from_value preserves the item.
        let value = serde_json::to_value(&item).expect("to_value");
        assert_eq!(value["json"], json!({ "k": "v" }));
        assert_eq!(value["paired_item"], json!(2));
        // `binary` is absent (skipped when None).
        assert!(value.get("binary").is_none());

        let back: Item = serde_json::from_value(value).expect("from_value");
        assert_eq!(item, back);
    }

    #[test]
    fn deserializes_from_json_only() {
        // Missing `binary` and `paired_item` fall back to their defaults.
        let item: Item = serde_json::from_str(r#"{"json":{"a":1}}"#).expect("deserialize");
        assert_eq!(item, Item::new(json!({ "a": 1 })));
    }
}
