extern crate bindgen;

use std::env;
use std::path::PathBuf;

fn main() {
    // Tell cargo to tell rustc to link the system cups
    // shared library.
    println!("cargo:rustc-link-lib=cups");

    // The bindgen::Builder is the main entry point
    // to bindgen, and lets you build up options for
    // the resulting bindings.
    let bindings = bindgen::Builder::default()
        .rust_target(bindgen::RustTarget::stable(85, 0).map_err(|_| ()).unwrap())
        .rust_edition(bindgen::RustEdition::Edition2024)
        .wrap_unsafe_ops(true)

        // The input header we would like to generate
        // bindings for.
        .header("wrapper.h")
        // These fail with size issues.
        .opaque_type("__msfilterreq")
        .opaque_type("group_req")
        .opaque_type("group_source_req")
        // bindgen layout tests fail on Rust nightly >1.21.0
        // "thread 'bindgen_test_layout_max_align_t' panicked at
        // 'assertion failed: `(left == right)`"
        .layout_tests(false)
        // This fails with "`IPPORT_RESERVED` already defined" on Linux.
        .blocklist_item("IPPORT_RESERVED")

        // Finish the builder and generate the bindings.
        .generate()
        // Unwrap the Result and panic on failure.
        .expect("Unable to generate bindings");

    // Write the bindings to the $OUT_DIR/bindings.rs file.
    let out_path = PathBuf::from(env::var("OUT_DIR").unwrap());
    bindings
        .write_to_file(out_path.join("bindings.rs"))
        .expect("Couldn't write bindings!");
}
