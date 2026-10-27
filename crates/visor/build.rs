fn main() {
    let config = slint_build::CompilerConfiguration::new().with_style("fluent-dark".into());
    slint_build::compile_with_config("ui/app.slint", config).expect("Slint UI failed to compile");

    // The app icon and version info in visor.exe. Needs the Windows SDK's
    // resource compiler, so only when building on Windows (not when
    // cross-checking from another OS).
    #[cfg(windows)]
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        println!("cargo:rerun-if-changed=../../packaging/windows/visor.ico");
        let mut res = winresource::WindowsResource::new();
        res.set_icon("../../packaging/windows/visor.ico")
            .set("ProductName", "Visor")
            .set("FileDescription", "Visor screen recorder");
        res.compile().expect("embedding the Windows icon failed");
    }
}
