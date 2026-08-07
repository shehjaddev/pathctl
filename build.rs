//! Embeds `pathctl.manifest` (longPathAware + Windows 10/11 compatibility)
//! into the binary. On MSVC this uses linker manifest-embedding flags, so no
//! `rc.exe` is required.

fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        embed_manifest::embed_manifest_file("pathctl.manifest")
            .expect("failed to embed pathctl.manifest");
        println!("cargo:rerun-if-changed=pathctl.manifest");
    }
    println!("cargo:rerun-if-changed=build.rs");
}
