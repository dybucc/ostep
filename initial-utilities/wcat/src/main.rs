use std::env;
use std::io::Result;
use std::path::PathBuf;

type Args = Vec<PathBuf>;

fn main() -> Result<()> {
    let proc_args: Args = env::args_os().skip(1).map(|e| {
        let new = PathBuf::from(e);

        new.try_exists().and_then(|e| e.then_some().ok())
    }).collect::<Result<Vec<_>, _>>();

    Ok(())
}
