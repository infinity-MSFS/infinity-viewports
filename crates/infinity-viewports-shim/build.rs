fn main() {
    println!("cargo:rerun-if-changed=csrc/seh_shim.c");
    let target = std::env::var("TARGET").unwrap_or_default();
    if !target.contains("windows") || !target.contains("msvc") {
        println!("cargo:warning=rtt-shim: SEH shim not built for non-MSVC target {target}");
        return;
    }
    let mut build = cc::Build::new();
    build.file("csrc/seh_shim.c");
    build.compile("rtt_shim");
}
