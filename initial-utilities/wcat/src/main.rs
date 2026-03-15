use std::{env, fs, io::Error, path::Path, process::ExitCode};

fn main() -> ExitCode {
    match env::args_os().skip(1).try_fold((), |_, e| {
        print!("{}", fs::read_to_string(Path::new(&e))?);

        Ok::<_, Error>(())
    }) {
        | Ok(_) => ExitCode::SUCCESS,
        | Err(_) => {
            println!("wcat: cannot open file");

            ExitCode::FAILURE
        },
    }
}
