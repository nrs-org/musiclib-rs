//! Feature `vulkan`: compile the llama.cpp C shim and link llama.cpp, found
//! with pkg-config (`llama.pc`; the dev shell provides nixpkgs'
//! `llama-cpp-vulkan`). Nothing to do without the feature.

fn main() {
    #[cfg(feature = "vulkan")]
    vulkan();
}

#[cfg(feature = "vulkan")]
fn vulkan() {
    println!("cargo:rerun-if-changed=src/matcher/llama_shim.c");
    // Include paths first; link flags are emitted after the shim so the
    // linker sees llama.cpp after the static shim that needs it.
    let lib = pkg_config::Config::new()
        .cargo_metadata(false)
        .probe("llama")
        .expect(
            "feature `vulkan` needs llama.cpp's llama.pc on PKG_CONFIG_PATH (use the dev shell)",
        );
    let mut shim = cc::Build::new();
    shim.file("src/matcher/llama_shim.c");
    for dir in &lib.include_paths {
        shim.include(dir);
    }
    shim.compile("llama_shim");
    pkg_config::Config::new().probe("llama").expect("llama.pc");
    for dir in &lib.link_paths {
        println!("cargo:rustc-link-arg=-Wl,-rpath,{}", dir.display());
    }
    // Builds that load ggml backends at runtime keep them in <prefix>/bin.
    if let Ok(prefix) = pkg_config::get_variable("llama", "prefix") {
        println!("cargo:rustc-env=LLAMA_BACKEND_DIR={prefix}/bin");
    }
}
