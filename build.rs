fn main() {
    println!("cargo:rerun-if-changed=assets/net-combiner-icon.ico");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut resource = winresource::WindowsResource::new();
        resource.set_icon("assets/net-combiner-icon.ico");
        resource.set("FileDescription", "net-combiner");
        resource.set("ProductName", "net-combiner");
        resource.set("OriginalFilename", "net-combiner.exe");
        resource
            .compile()
            .expect("failed to embed Windows application resources");
    }
}
