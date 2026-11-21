use std::{env, path::PathBuf};

fn main() {
    tauri_build::build();

    // Tests need the same Common Controls v6 manifest as the app binary.
    if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows")
        && let Ok(out_dir) = env::var("OUT_DIR")
    {
        let resource = PathBuf::from(out_dir).join("libresource.a");
        if resource.exists() {
            println!("cargo:rustc-link-arg={}", resource.display());
        }
    }
}
