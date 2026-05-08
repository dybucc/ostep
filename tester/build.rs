use build_rs::output;

fn main() {
    output::rustc_check_cfgs(&["trace"]);
}
