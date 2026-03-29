use std::{env, fs, io::Error, path::Path, process::ExitCode};

fn main() -> ExitCode {
  #![expect(clippy::unit_arg, reason = "Beauty comes at cost.")]

  match env::args_os().skip(1).try_for_each(|e| {
    Ok::<_, Error>(print!("{}", fs::read_to_string(Path::new(&e))?))
  }) {
    | Ok(_) => ExitCode::SUCCESS,
    | Err(_) => (println!("wcat: cannot open file"), ExitCode::FAILURE).1,
  }
}
