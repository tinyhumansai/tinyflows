use super::*;
use crate::caps::mock::mock_capabilities;
use serde_json::json;

#[test]
fn code_language_is_copy_and_comparable() {
    let js = CodeLanguage::JavaScript;
    let copied = js; // `Copy`, so `js` remains usable below.
    assert_eq!(js, copied);
    assert_eq!(js, CodeLanguage::JavaScript);
    assert_ne!(CodeLanguage::JavaScript, CodeLanguage::Python);
}

#[test]
fn code_language_debug_names_the_variant() {
    assert_eq!(format!("{:?}", CodeLanguage::JavaScript), "JavaScript");
    assert_eq!(format!("{:?}", CodeLanguage::Python), "Python");
}

#[test]
fn capabilities_bundle_is_cloneable_and_shares_impls() {
    let caps = mock_capabilities();
    let clone = caps.clone();
    // Cloning shares the underlying `Arc`s rather than deep-copying.
    assert!(Arc::ptr_eq(&caps.llm, &clone.llm));
    assert!(Arc::ptr_eq(&caps.tools, &clone.tools));
    assert!(Arc::ptr_eq(&caps.http, &clone.http));
    assert!(Arc::ptr_eq(&caps.code, &clone.code));
    assert!(Arc::ptr_eq(&caps.state, &clone.state));
    assert!(Arc::ptr_eq(&caps.resolver, &clone.resolver));
    assert!(Arc::ptr_eq(
        caps.memory.as_ref().expect("mock wires memory"),
        clone.memory.as_ref().expect("mock wires memory")
    ));
}

#[tokio::test]
async fn capabilities_dispatch_through_trait_objects() {
    // Exercises the bundle purely through the trait-object surface.
    let caps = mock_capabilities();
    let out = caps
        .llm
        .complete(json!({"prompt": "hi"}), None)
        .await
        .unwrap();
    assert_eq!(out["completion"]["prompt"], "hi");
}
