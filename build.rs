fn main() {
    // Ask Windows for the modern (themed) common controls in the GUI;
    // without this manifest entry, buttons and lists look like Windows 95.
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        println!("cargo:rustc-link-arg-bin=milercast=/MANIFEST:EMBED");
        println!(
            "cargo:rustc-link-arg-bin=milercast=/MANIFESTDEPENDENCY:type='win32' \
             name='Microsoft.Windows.Common-Controls' version='6.0.0.0' \
             processorArchitecture='*' publicKeyToken='6595b64144ccf1df' language='*'"
        );
    }
}
