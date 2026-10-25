//! The C++ runtime (for Luau) is linked statically on Android; see .cargo/config.toml.
//! libc++_static does not contain the C++ ABI layer (exceptions, RTTI,
//! std::terminate), so libc++abi is added at the end of the link line.
fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("android") {
        println!("cargo:rustc-link-arg=-lc++abi");
    }
}
