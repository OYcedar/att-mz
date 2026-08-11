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
    // 恢复 WriteBack 标点与排版后，Debug 构建的生产入口在 Translate 路径会超过 Windows
    // 默认 1 MiB 主栈；2 MiB 已由真实进程测试验证，且不把内部栈容量暴露成配置。
    println!("cargo::rustc-link-arg-bin=att=/STACK:2097152");
}
