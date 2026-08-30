use super::*;

#[test]
fn product_name_matches_crate_name() {
    assert_eq!(product_name(), "tinyflows");
}

#[test]
fn exposes_package_version() {
    assert!(!VERSION.is_empty());
}

#[test]
fn version_looks_like_semver() {
    // At least `major.minor.patch`, each a run of digits.
    let core = VERSION.split(['-', '+']).next().expect("version core");
    let parts: Vec<&str> = core.split('.').collect();
    assert!(
        parts.len() >= 3,
        "version {VERSION:?} should have at least major.minor.patch"
    );
    for part in &parts[..3] {
        assert!(
            !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()),
            "version {VERSION:?} component {part:?} should be numeric"
        );
    }
}

#[test]
fn crate_name_and_product_name_agree() {
    assert_eq!(CRATE_NAME, "tinyflows");
    assert_eq!(product_name(), CRATE_NAME);
}
