fn main() {
    // screencapturekit's Swift bridge links the system Swift runtime; link args
    // from a dependency's build script don't reach this binary, so add the rpath here.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rustc-link-arg-bins=-Wl,-rpath,/usr/lib/swift");
    }
}
