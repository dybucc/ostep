fn main() {
    println!("cargo::rustc-check-cfg=cfg(trace, values(none()))");
}
