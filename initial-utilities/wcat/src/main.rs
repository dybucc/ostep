use std::{env, fs, path::Path, process::ExitCode};

fn main() -> ExitCode {
    let args = env::args_os();
    let args_iter = args.skip(1);
    for arg in args_iter {
        let path = Path::new(&arg);
        let file_contents = {
            let res = fs::read_to_string(path);
            if let Ok(file_contents) = res {
                file_contents
            } else {
                println!("wcat: cannot open file");
                return ExitCode::FAILURE;
            }
        };
        print!("{file_contents}");
    }
    ExitCode::SUCCESS
}
