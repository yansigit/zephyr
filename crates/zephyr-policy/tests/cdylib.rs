//! Builds the cdylib and checks that it exports the rpi registration symbol.

mod common;

#[test]
fn cdylib_builds_and_exports_registration_symbol() {
    let lib = common::build_cdylib();
    let bytes = std::fs::read(&lib).unwrap_or_else(|e| panic!("read {}: {e}", lib.display()));
    // The name appears in the dynamic symbol table as a NUL-terminated string.
    let symbol = b"rpi_plugin_register\0";
    assert!(
        bytes.windows(symbol.len()).any(|w| w == symbol),
        "{} does not export rpi_plugin_register",
        lib.display()
    );
}
