fn main() {
    tauri_build::build();

    // Tauri attaches its Windows resource object (icon + manifest with the
    // Common-Controls v6 dependency) to bin targets via `rustc-link-arg-bins`,
    // which cargo only applies to bin targets. Test binaries would otherwise
    // fall back to the mingw CRT `default-manifest.o`, whose manifest lacks
    // Common-Controls: comctl32 then resolves to v5.82, which does not export
    // `TaskDialogIndirect`, and the test process dies at load with
    // STATUS_ENTRYPOINT_NOT_FOUND (0xc0000139). Link the same resource object
    // into every final link unit — it is a plain COFF object, so the linker
    // includes it fully, and placed before the CRT endfile its manifest wins.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        if let Ok(out_dir) = std::env::var("OUT_DIR") {
            let resource = std::path::Path::new(&out_dir).join("libresource.a");
            if resource.exists() {
                println!("cargo:rustc-link-arg={}", resource.display());
            }
        }
    }
}
