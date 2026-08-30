use super::*;

#[test]
fn a_token_is_thirty_two_hex_characters() {
    let token = token();

    assert_eq!(token.len(), 32, "{token}");
    assert!(token.bytes().all(|b| b.is_ascii_hexdigit()), "{token}");
}

#[test]
fn two_tokens_differ() {
    // The whole contract. A repeat here means two writers can name the same
    // scratch file and scribble over each other.
    assert_ne!(token(), token());
}

#[test]
fn a_token_is_one_path_component() {
    let token = token();

    assert_eq!(std::path::Path::new(&token).components().count(), 1);
}
