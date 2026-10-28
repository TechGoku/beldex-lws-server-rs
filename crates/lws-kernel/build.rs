//! Builds the C++ scan kernel (and beldex core under it) with CMake, or links a
//! prebuilt one when `LWS_KERNEL_LIB_DIR` is set.

use std::env;
use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-env-changed=LWS_KERNEL_LIB_DIR");

    let lib_dir = match env::var_os("LWS_KERNEL_LIB_DIR") {
        Some(dir) => {
            // Stage the prebuilt library inside OUT_DIR: cargo puts OUT_DIR
            // search paths on the loader path for `cargo run` and `cargo test`.
            let staged = PathBuf::from(env::var("OUT_DIR").unwrap()).join("lib");
            std::fs::create_dir_all(&staged).unwrap();
            let name = "libbeldex_lws_kernel.so";
            let source = PathBuf::from(dir).join(name);
            println!("cargo:rerun-if-changed={}", source.display());
            std::fs::copy(&source, staged.join(name))
                .unwrap_or_else(|e| panic!("cannot stage {}: {e}", source.display()));
            staged
        }
        None => {
            let kernel = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap()).join("../../kernel");
            for path in ["CMakeLists.txt", "src", "include", "../.gitmodules"] {
                println!("cargo:rerun-if-changed={}", kernel.join(path).display());
            }
            let profile = if env::var("PROFILE").as_deref() == Ok("release") { "Release" } else { "RelWithDebInfo" };
            let out = cmake::Config::new(&kernel)
                .profile(profile)
                .define("LWS_KERNEL_TESTS", "OFF")
                .build_target("beldex_lws_kernel")
                .build();
            out.join("build")
        }
    };

    println!("cargo:rustc-link-search=native={}", lib_dir.display());
    println!("cargo:rustc-link-lib=dylib=beldex_lws_kernel");
    // Exposed to dependents (DEP_BELDEX_LWS_KERNEL_LIB_DIR) so binaries can embed
    // an rpath to it for development builds.
    println!("cargo:lib_dir={}", lib_dir.display());
}
