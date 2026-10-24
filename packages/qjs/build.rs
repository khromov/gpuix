fn main() {
    // gpuix-native's napi bindings compile in with it and reference `napi_*`
    // symbols that only Node provides; nothing here ever calls them, so let the
    // executable link with them unresolved, the way a `.node` cdylib does.
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if os == "macos" {
        println!("cargo:rustc-link-arg-bins=-Wl,-undefined,dynamic_lookup");
    }
}
