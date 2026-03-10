use std::{env, fs, io::Error, path::PathBuf, process::ExitCode};

fn main() -> ExitCode {
    match env::args_os().skip(1).try_fold((), |_, e| {
        let path = PathBuf::from(e).canonicalize()?;
        print!("{}", fs::read_to_string(path)?);

        Ok::<_, Error>(())
    }) {
        | Ok(_) => ExitCode::SUCCESS,
        | Err(_) => {
            eprintln!("wcat: cannot open file");
            ExitCode::FAILURE
        },
    }
}
