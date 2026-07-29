use std::path::PathBuf;

fn main() {
    let manifest = PathBuf::from(
        std::env::var_os("CARGO_MANIFEST_DIR").expect("Cargo 必须提供 CARGO_MANIFEST_DIR"),
    )
    .join("att.exe.manifest");

    println!("cargo::rerun-if-changed={}", manifest.display());
    println!("cargo::rustc-link-arg-bin=att=/MANIFEST:EMBED,ID=1");
    println!(
        "cargo::rustc-link-arg-bin=att=/MANIFESTINPUT:{}",
        manifest.display()
    );
}
