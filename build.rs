fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rustc-link-arg-bin=battery-tray=-nostdlib");
    println!(
        "cargo:rustc-link-arg-bin=battery-tray=-Wl,--subsystem,windows,-e,mainCRTStartup,--gc-sections"
    );
}
