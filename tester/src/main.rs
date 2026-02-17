use std::fs;

use anyhow::{Context, Ok, Result, anyhow};

#[derive(Debug)]
struct Test {
    num: usize,
    out: String,
    err: String,
    rc: String,
    run: String,
    desc: String,
    pre: String,
    post: String,
}

impl Default for Test {
    fn default() -> Self {
        Self {
            num: 1,
            out: String::default(),
            err: String::default(),
            rc: String::default(),
            run: String::default(),
            desc: String::default(),
            pre: String::default(),
            post: String::default(),
        }
    }
}

fn main() -> Result<()> {
    let mut tests = fs::read_dir("./tests")?.try_fold(Vec::new(), |mut accum, entry| {
        let entry = entry?;
        if entry.metadata()?.is_file() {
            let entry_path = entry.path();
            let entry_num = entry_path
                .file_stem()
                .ok_or(
                    anyhow!("file doesn't contain file stem")
                        .context("expected file stem to be a numeric value denoting the test"),
                )?
                .to_str()
                .ok_or(
                    anyhow!("file contains non-utf8 codepoints")
                        .context("expected numeric values for each test"),
                )?
                .parse::<usize>()
                .context("expected file to denote a test number in the suite")?;

            accum.push((entry_path, entry_num));
        }

        Ok(accum)
    })?;

    tests.sort_unstable_by_key(|&(_, test_num)| test_num);

    let tests = tests.iter().try_fold(
        (Vec::with_capacity(tests.len()), Test::default()),
        |(mut tests, mut current_test), (test_path, test_num)| {
            if current_test.num != *test_num {
                tests.push(current_test);

                current_test = Test::default();
                current_test.num = *test_num;
            }

            match test_path
                .extension()
                .ok_or(anyhow!("file doesn't contain extension").context(
                    "expected one of `rc`, `err`, `out`, `in` (and variations,) `desc`, `pre` or \
                    `post` test suite element extensions",
                ))?
                .as_encoded_bytes()
            {
                b"rc" => current_test.rc = fs::read_to_string(test_path.canonicalize()?)?,
                b"out" => current_test.out = fs::read_to_string(test_path.canonicalize()?)?,
                b"err" => current_test.err = fs::read_to_string(test_path.canonicalize()?)?,
                b"run" => current_test.run = fs::read_to_string(test_path.canonicalize()?)?,
                b"desc" => current_test.desc = fs::read_to_string(test_path.canonicalize()?)?,
                b"pre" => current_test.pre = fs::read_to_string(test_path.canonicalize()?)?,
                b"post" => current_test.post = fs::read_to_string(test_path.canonicalize()?)?,
                _ => (),
            }

            Ok((tests, current_test))
        },
    )?;

    // 1. Execute the program in the current Cargo project.
    //    - Set up a virtual environment to install the program in the current
    //      Cargo project and have the commands in the .run files be run more
    //      easily.
    // 2. Retrieve the return error of the program and check that it matches the
    //    one in the corresponding .rc file.
    //    3. If it matches, continue on step 5..
    //    4. If it doesn't match, report the error and exit out of the program.
    // 5. Check that the stdout of the program matches with the corresponding
    //    .out file.
    //    6. If it matches, continue from step 8.
    //    7. If it doesn't match, report the error and exit out of the program.
    // 8. Check that the stderr of the program matches with the corresponding
    //    .err file.
    //    9. If it matches, continue from step 11.
    //    10. If it doesn't match, report the error and exit out of the program.

    Ok(())
}
