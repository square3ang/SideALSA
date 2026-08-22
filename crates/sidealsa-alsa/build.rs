fn main() {
    cc::Build::new()
        .file("src/plugin.c")
        .define("PIC", None)
        .flag_if_supported("-std=c11")
        .warnings(true)
        .compile("sidealsa_pcm_plugin");
    println!("cargo:rustc-link-lib=dylib=asound");
}
