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
#[path = "data_tests.rs"]
mod tests;
