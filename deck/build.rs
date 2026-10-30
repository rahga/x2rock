fn main() {
    slint_build::compile("ui/app.slint").expect("ui/app.slint compiles");
    // The icon Explorer shows for the .exe.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        embed_resource::compile("dist/x2rock-deck.rc", embed_resource::NONE)
            .manifest_optional()
            .expect("the Windows icon resource compiles");
    }
}
