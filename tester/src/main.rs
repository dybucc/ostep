#![allow(unstable_features)]

use std::{
    env, fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Ok, Result, anyhow, bail};
use cargo_metadata::MetadataCommand;
use clap::Parser;

#[derive(Debug)]
struct Test {
    num: usize,
    out: Option<String>,
    err: Option<String>,
    rc: Option<String>,
    run: Option<String>,
    desc: Option<String>,
    pre: Option<String>,
    post: Option<String>,
}

impl Default for Test {
    fn default() -> Self {
        Self {
            num: 1,
            out: Option::default(),
            err: Option::default(),
            rc: Option::default(),
            run: Option::default(),
            desc: Option::default(),
            pre: Option::default(),
            post: Option::default(),
        }
    }
}

/// Test harness for OSTEP projects (or anything that aligns with the OSTEP
/// testing practices.)
#[derive(Debug, Parser)]
#[command(
    version,
    about,
    long_about = None,
    next_line_help = true,
    disable_help_flag = true,
    disable_help_subcommand = true,
    infer_long_args = true
)]
struct Args {
    #[arg(short, long)]
    /// Specify the package to work on if we are in a workspace root and no
    /// package could be determined.
    package: String,
}

fn main() -> Result<()> {
    let workspace_metadata = MetadataCommand::new()
        .no_deps()
        .other_options(["--format-version=1"])
        .exec()
        .context("failed to query cargo workspace/package during initialization")?;
    let current_dir = env::current_dir().context("failed to query pwd during initialization")?;
    let workspace_packages = workspace_metadata.workspace_packages();
    if workspace_packages.len() > 1
        && let Some(&pkg) = workspace_packages.iter().find(|&pkg| {
            let path = pkg.manifest_path.as_std_path().parent().expect(
                "`cargo metadata` specifies an absolute path by default and the `manifest_path` \
                field is part of the stable commitment of `format-version=1`; hopefully you're not \
                running `tester` from `/`",
            );

            path == current_dir
        })
    {
        let cmd_pkg = Args::parse().package;
        if pkg.name == cmd_pkg {
            env::set_current_dir(Path::new(&pkg.manifest_path))
                .context("failed to cwd to the issued package's directory")?;
        }
    }
    let mut tests = fs::read_dir("./tests")
        .context(
            "the `tests` directory should be present in the folder where you're running the \
            program",
        )?
        .try_fold(Vec::new(), |mut accum, entry| {
            let entry = entry.context(
                "failed to read entry in the `tests` directory when parsing `tests` directory \
                entries",
            )?;
            let entry_path = entry.path();
            let entry_metadata = entry.metadata().with_context(|| {
                format!(
                    "failed to read entry fs metadata when parsing `tests` directory entry: `{}`",
                    entry_path.display()
                )
            })?;
            let entry_extension = entry_path
                .extension()
                .ok_or(anyhow!(
                    "failed to fetch file extension for `tests` file entry"
                ))
                .with_context(|| {
                    format!(
                        "failed to fetch fs entry extensions when parsing `tests` directory \
                            entry: `{}`",
                        entry_path.display()
                    )
                })?;
            if entry_metadata.is_file()
                && matches!(
                    entry_extension.as_encoded_bytes(),
                    b"out" | b"err" | b"rc" | b"run" | b"desc" | b"pre" | b"post"
                )
            {
                let entry_num = entry_path
                    .file_stem()
                    .ok_or(anyhow!("file doesn't contain file stem"))
                    .context("expected file stem to be a numeric value denoting the test")
                    .with_context(|| {
                        format!(
                            "failed when parsing `tests` directory entry: `{}`",
                            entry_path.display()
                        )
                    })?
                    .to_str()
                    .ok_or(anyhow!("file contains non-utf8 codepoints"))
                    .context(
                        "expected utf-8-compliant values for each test; each test should denote a \
                        numeric value",
                    )
                    .with_context(|| {
                        format!(
                            "failed when parsing `tests` directory entry: `{}`",
                            entry_path.display()
                        )
                    })?
                    .parse::<usize>()
                    .context("expected file to denote a test number in the suite")
                    .with_context(|| {
                        format!(
                            "failed when parsing `tests` directory entry: `{}`",
                            entry_path.display()
                        )
                    })?;
                let entry_extension = entry_extension.to_os_string();
                accum.push((entry_path, entry_num, entry_extension));
            }

            Ok(accum)
        })?;
    tests.sort_unstable_by_key(|&(_, test_num, _)| test_num);
    let tests = tests
        .iter()
        .try_fold(
            (Vec::with_capacity(tests.len()), Test::default()),
            |(mut tests, mut current_test), (test_path, test_num, test_extension)| {
                if current_test.num != *test_num {
                    tests.push(current_test);
                    current_test = Test::default();
                    current_test.num = *test_num;
                }

                macro_rules! check_entry {
                    ($test:expr) => {{
                        Some(
                            fs::read_to_string($test.canonicalize().with_context(|| {
                                format!(
                                    "failed while parsing `tests` directory entry `{}`",
                                    test_path.display()
                                )
                            })?)
                            .with_context(|| {
                                format!(
                                    "failed while parsing `tests` directory entry `{}`",
                                    test_path.display()
                                )
                            })?,
                        )
                    }};
                }

                match test_extension.as_encoded_bytes() {
                    b"rc" => current_test.rc = check_entry!(test_path),
                    b"out" => current_test.out = check_entry!(test_path),
                    b"err" => current_test.err = check_entry!(test_path),
                    b"run" => current_test.run = check_entry!(test_path),
                    b"desc" => current_test.desc = check_entry!(test_path),
                    b"pre" => current_test.pre = check_entry!(test_path),
                    b"post" => current_test.post = check_entry!(test_path),
                    _ => (),
                }

                Ok((tests, current_test))
            },
        )?
        .0;

    // 1. Query the directory with `cargo metadata`.
    // 2. Parse information from the Cargo project to check whether it's a
    //    workspace, of if it's an individual package.
    //    3. If it's a regular package, then proceed as usual with the already
    //       implemented functionality.
    //    4. If it's a workspace, make sure the user passed in a command line
    //       argument that specifies the package that they want to test.
    //       If it's a workspace member's directory that we are in, skip the
    //       error about the `-p` flag not being issued.
    // 5. With the known location to the package, change this process' pwd to
    //    the package's manifest file (`Cargo.toml`) path.
    // 6. Proceed with the already implemented functionality for parsing entries
    //    of the `./tests` directory in the pwd of this process.
    // 7. Run `cargo build --release --message-format=json`, and parse the
    //    path of the resulting binary under the key `executable`.
    // 8. Copy over the executable parsed to the process' pwd.
    // 9. For each test:
    //    10. Run the same command as indicated in the `.rc` file.
    //    11. Check the exit status matches the contents of the `.rc` file.
    //    12. Check the stdout matches the contents of the `.out` file.
    //    13. Check the stderr matches the contents of the `.err` file.

    Ok(())
}
