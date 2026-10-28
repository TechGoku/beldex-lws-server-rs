//! Lets the binary find libbeldex_lws_kernel.so next to itself (`$ORIGIN`,
//! for packaged installs) or in the kernel build directory (development).

fn main() {
    println!("cargo:rustc-link-arg-bins=-Wl,-rpath,$ORIGIN");
    println!("cargo:rustc-link-arg-bins=-Wl,-rpath,$ORIGIN/../lib");
    if let Ok(dir) = std::env::var("DEP_BELDEX_LWS_KERNEL_LIB_DIR") {
        println!("cargo:rustc-link-arg-bins=-Wl,-rpath,{dir}");
    }
}
