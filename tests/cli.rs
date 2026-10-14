//! Runs the built binary, so `src/main.rs` is exercised and covered.

use std::process::Command;

#[test]
fn prints_hello_world() {
    let output = Command::new(env!("CARGO_BIN_EXE_airmate"))
        .output()
        .expect("failed to run the airmate binary");

    assert!(output.status.success());
    assert_eq!(String::from_utf8_lossy(&output.stdout), "Hello, world!\n");
    assert!(output.stderr.is_empty());
}
