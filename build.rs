fn main() {
    println!("cargo:rerun-if-changed=assets/ilinkos.ico");
    if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
        let mut resource = winresource::WindowsResource::new();
        resource.set_icon("assets/ilinkos.ico");
        resource.compile().expect("failed to embed the app icon");
    }
}
