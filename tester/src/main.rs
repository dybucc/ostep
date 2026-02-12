use std::fs;

use anyhow::{Error, Ok, Result, anyhow};

fn main() -> Result<()> {
    let mut tests = fs::read_dir("./tests")?.try_fold(Vec::new(), |mut accum, entry| {
        let entry = entry?;
        if entry.metadata()?.is_file() {
            let entry_file = entry
                .path()
                .file_stem()
                .expect("")
                .to_str()
                .ok_or(anyhow!("problem"))?;
            accum.push((entry_file, entry_file.parse::<usize>()?));
        }

        Ok(accum)
    })?;

    tests.sort_unstable_by(|&(_, entry1), &(_, entry2)| entry1.cmp(&entry2));

    Ok(())
}
