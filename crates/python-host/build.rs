// The host links libpython dynamically. Runtime lookup, in order:
// - packaged layouts: macOS Hermes.app/Contents/Resources/python/lib, Linux <prefix>/lib/hermes/python/lib
//   (Windows: python3XX.dll next to the executable, copied there by package-windows.ps1);
// - in debug builds only, the interpreter the build linked against, so `cargo run` and `cargo test`
//   work unpackaged. Release builds carry no build-machine paths.
fn main() {
    let config = pyo3_build_config::get();
    let version = config.version();
    println!("cargo:rustc-env=HERMES_PYTHON_VERSION={}.{}", version.major, version.minor);
    let target = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    match target.as_str() {
        "macos" => println!("cargo:rustc-link-arg=-Wl,-rpath,@executable_path/../Resources/python/lib"),
        "linux" => println!("cargo:rustc-link-arg=-Wl,-rpath,$ORIGIN/../lib/hermes/python/lib"),
        _ => {}
    }
    if std::env::var("PROFILE").as_deref() != Ok("release") {
        pyo3_build_config::add_libpython_rpath_link_args();
        pyo3_build_config::add_python_framework_link_args();
        // An embedded interpreter whose executable is not python cannot always find its own
        // standard library (a relocatable build's compiled-in prefix is a placeholder).
        let home = config.executable().and_then(|python| std::process::Command::new(python).args(["-c", "import sys; print(sys.base_prefix)"]).output().ok());
        if let Some(home) = home.filter(|o| o.status.success()) {
            println!("cargo:rustc-env=HERMES_PYTHON_BUILD_HOME={}", String::from_utf8_lossy(&home.stdout).trim());
        }
    }
}
