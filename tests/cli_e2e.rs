//! CLI smoke tests for the tinyflows binary.

use std::process::Command;

#[test]
fn binary_prints_product_name() {
    let output = Command::new(env!("CARGO_BIN_EXE_tinyflows"))
        .output()
        .expect("run tinyflows binary");

    assert!(
        output.status.success(),
        "binary should exit successfully: {output:?}"
    );
    assert_eq!(
        String::from_utf8(output.stdout).expect("stdout utf8"),
        "tinyflows\n"
    );
    assert!(
        output.stderr.is_empty(),
        "binary should not write stderr: {:?}",
        String::from_utf8_lossy(&output.stderr)
    );
}
