fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        println!("cargo:rerun-if-changed=../../data/icons/tidemark.ico");
        // Both daemon and maintenance helper belong to this package and receive
        // its icon and product version. The helper runs from Inno's temp folder.
        winresource::WindowsResource::new()
            .set_icon("../../data/icons/tidemark.ico")
            .set("ProductName", "Tidemark")
            .set(
                "FileDescription",
                "Tidemark daemon and installation maintenance",
            )
            .compile()
            .expect("the Windows icon and version resources compile");
    }
}
