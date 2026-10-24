// Arti's dependencies import these DLLs, which pull in user32.dll at load
// time. The network process runs with Win32k lockdown, where user32 can't
// initialize, so they are delay-loaded: mapped only if something actually
// calls into them (and then the crash filter names the culprit).
fn main() {
    // shell32 is never called (third_party/tor-config-path), and release
    // builds drop the import entirely (LNK4199 says so), but debug builds
    // still import it, so it stays in the list.
    for dll in ["shell32.dll", "oleaut32.dll", "pdh.dll", "powrprof.dll"] {
        println!("cargo:rustc-link-arg-bins=/DELAYLOAD:{dll}");
    }
    println!("cargo:rustc-link-arg-bins=delayimp.lib");
}
