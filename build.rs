// Ensure libswift_Concurrency.dylib is resolvable at load time.
//
// screencapturekit compiles a Swift bridge and the resulting binary has an
// @rpath/libswift_Concurrency.dylib dependency.  screencapturekit's own
// build.rs emits -Wl,-rpath,/usr/lib/swift, but on some configurations
// (stale Cargo build cache, Command Line Tools-only installs) that rpath can
// be absent from the final binary, causing:
//
//   dyld: Library not loaded: @rpath/libswift_Concurrency.dylib
//
// This build script re-emits the rpath unconditionally so the system dyld
// shared-cache entry at /usr/lib/swift always resolves correctly.
//
// Do NOT add the Command Line Tools swift-5.5 path
// (/Library/Developer/CommandLineTools/usr/lib/swift-5.5/macosx). That
// directory contains a real .dylib file, and if both /usr/lib/swift AND the
// CLT path are in the rpath, dyld loads the library twice, producing ObjC
// "class implemented in both" warnings.  The system dyld cache version at
// /usr/lib/swift is always preferred on macOS 12+.

fn main() {
    println!("cargo:rerun-if-env-changed=DEVELOPER_DIR");

    // macOS 12+: libswift_Concurrency lives in the system dyld shared cache
    // under /usr/lib/swift.
    println!("cargo:rustc-link-arg=-Wl,-rpath,/usr/lib/swift");

    // Pre-macOS-12 fallback: the library shipped inside the Xcode toolchain.
    // Only add these paths on a full Xcode install (not CLT) to avoid
    // loading a second copy alongside the dyld cache version.
    if let Some(base) = xcode_developer_dir() {
        // Detect CLT-only: xcode-select -p returns a path under
        // /Library/Developer/CommandLineTools rather than an Xcode.app bundle.
        let is_clt = base.contains("CommandLineTools");
        if !is_clt {
            let tc = format!("{base}/Toolchains/XcodeDefault.xctoolchain");
            println!("cargo:rustc-link-arg=-Wl,-rpath,{tc}/usr/lib/swift-5.5/macosx");
            println!("cargo:rustc-link-arg=-Wl,-rpath,{tc}/usr/lib/swift/macosx");
        }
    }
}

fn xcode_developer_dir() -> Option<String> {
    let out = std::process::Command::new("xcode-select")
        .arg("-p")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8(out.stdout).ok()?.trim().to_string())
}
