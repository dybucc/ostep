#![feature(exit_status_error, string_from_utf8_lossy_owned, bool_to_result)]

use std::{
    borrow::Cow,
    env,
    ffi::{OsStr, OsString},
    fmt::Display,
    fs,
    ops::ControlFlow,
    os::unix::process::ExitStatusExt,
    path::PathBuf,
    process::{Command, ExitStatus},
};

use anyhow::{Context, Ok, Result, anyhow, bail};
use cargo_metadata::MetadataCommand;
use clap::Parser;
use serde_json::Value;

#[derive(Debug)]
struct Test {
    num:  usize,
    out:  Option<String>,
    err:  Option<String>,
    rc:   Option<String>,
    run:  Option<String>,
    desc: Option<String>,
    pre:  Option<String>,
    post: Option<String>,
}

impl Default for Test {
    fn default() -> Self {
        Self {
            num:  1,
            out:  Option::default(),
            err:  Option::default(),
            rc:   Option::default(),
            run:  Option::default(),
            desc: Option::default(),
            pre:  Option::default(),
            post: Option::default(),
        }
    }
}

#[derive(Debug, Parser)]
#[command(
    version,
    about,
    long_about = None,
    disable_help_flag = true,
    disable_help_subcommand = true,
    infer_long_args = true
)]
/// Test harness for OSTEP projects (or anything that aligns with the OSTEP
/// testing practices.)
struct Args {
    #[arg(short, long)]
    /// Specify the package to work on in a workspace.
    package: Option<String>,
}

fn main() -> Result<()> {
    let (target_pkg, mut tests) = (find_pkg()?, find_tests()?);
    tests.sort_unstable_by_key(|&(_, test_num, _)| test_num);
    let (exe, tests) = (copy_exe(&target_pkg)?, produce_tests(tests)?);

    run_tests(exe, tests, &target_pkg)
}

fn find_pkg() -> Result<String> {
    let workspace_metadata = MetadataCommand::new()
        .no_deps()
        .exec()
        .context("failed to query cargo workspace/package during initialization")?;
    let workspace_packages = workspace_metadata.workspace_packages();

    Ok(match workspace_packages.len() {
        | 2.. => {
            let pwd = env::current_dir().context("failed to fetch pwd during initialization")?;
            let arg_pkg_name = Args::parse().package;

            if let ControlFlow::Break(pkg) =
                workspace_packages.iter().try_fold(None, |_: Option<()>, pkg| {
                    if let Some(path) = pkg.manifest_path.parent()
                        && path == pwd
                    {
                        return ControlFlow::Break(pkg);
                    }
                    if let Some(name) = &arg_pkg_name
                        && pkg.name == name
                    {
                        return ControlFlow::Break(pkg);
                    }

                    ControlFlow::Continue(None)
                })
            {
                let pkg_path = pkg
                    .manifest_path
                    .parent()
                    .ok_or(anyhow!("pkg manifest path doesn't have a parent dir"))
                    .with_context(|| format!("failed while processing package: {}", pkg.name))?;
                env::set_current_dir(pkg_path)
                    .context("failed to set pwd during initialization")
                    .with_context(|| {
                        format!("failed to set pwd to pkg manifest path: {pkg_path}")
                    })?;

                pkg.name.to_string()
            } else {
                bail!(
                    "cargo workspace package directory doesn't match pwd and no `-p` package \
                     option was provided"
                );
            }
        },
        | 1 => {
            let target_pkg = workspace_packages.first().unwrap();
            let target_pkg_path = target_pkg
                .manifest_path
                .parent()
                .ok_or(anyhow!("pkg manifest path doesn't have a parent dir"))
                .with_context(|| format!("failed while processing package: {}", target_pkg.name))?;
            env::set_current_dir(target_pkg_path)
                .context("failed to set pwd during initialization")
                .with_context(|| {
                    format!("failed to set pwd to cargo package directory: {target_pkg_path}")
                })?;

            target_pkg.name.to_string()
        },
        | _ => bail!("no packages found"),
    })
}

fn find_tests() -> Result<Vec<(PathBuf, usize, OsString)>> {
    fs::read_dir("./tests")
        .context(
            "the `tests` directory should be present in the folder where you're running the \
             binary",
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
            if entry_metadata.is_file()
                && let Some(entry_extension) = entry_path.extension()
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
        })
}

fn produce_tests(tests: Vec<(PathBuf, usize, OsString)>) -> Result<Vec<Test>> {
    Ok(tests
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
                    | b"rc" => current_test.rc = check_entry!(test_path),
                    | b"out" => current_test.out = check_entry!(test_path),
                    | b"err" => current_test.err = check_entry!(test_path),
                    | b"run" => current_test.run = check_entry!(test_path),
                    | b"desc" => current_test.desc = check_entry!(test_path),
                    | b"pre" => current_test.pre = check_entry!(test_path),
                    | b"post" => current_test.post = check_entry!(test_path),
                    | _ =>
                        unreachable!("all file extensions have been filtered past `find_tests()`"),
                }

                Ok((tests, current_test))
            },
        )?
        .0)
}

fn preprocess_build_json(input: Vec<u8>) -> String {
    let mut output = String::from_utf8_lossy_owned(input);
    let mut stopper = output.len();
    let mut char_counter = 0;
    for line in output.lines() {
        if line == "}" {
            stopper = char_counter + 1;
            break;
        }
        // +1 to account for the line terminators that `lines()` munches.
        char_counter += line.len() + 1;
    }
    output.truncate(stopper);

    output
}

fn parse_build_json(input: Value) -> Option<PathBuf> {
    if let Value::Object(map) = input {
        if let Some(Value::String(s)) = map.get("executable") {
            Some(PathBuf::from(s))
        } else {
            None
        }
    } else {
        None
    }
}

fn copy_exe<T: Display>(target_pkg: &T) -> Result<String> {
    let exe =
        parse_build_json(
            serde_json::from_str(&preprocess_build_json(
                Command::new("cargo")
                    .args(["build", "--release", "--message-format=json"])
                    .output()
                    .context("failed to spawn `cargo-build` on pwd")
                    .with_context(|| {
                        format!(
                            "failed while trying to build binary for cargo workspace package: \
                             {target_pkg}",
                        )
                    })?
                    .exit_ok()
                    .context("package compilation through `cargo-build` failed")
                    .with_context(|| {
                        format!(
                            "failed while trying to build binary for cargo workspace package: \
                             {target_pkg}",
                        )
                    })?
                    .stdout,
            ))
            .context("failed to parse json output from `cargo-build`")
            .with_context(|| {
                format!(
                    "failed while trying to build binary for cargo workspace package: {target_pkg}",
                )
            })?,
        )
        .ok_or(anyhow!("failed to find `executable` entry in cargo build json output"))
        .with_context(|| {
            format!("failed while trying to build binary for cargo workspace package: {target_pkg}")
        })?;
    Command::new("cp")
        .args([exe.as_os_str(), OsStr::new(".")])
        .status()
        .context("failed to copy binary executable to pwd")
        .with_context(|| {
            format!("failed while managing binary for cargo workspace package: {target_pkg}")
        })?;

    Ok(Cow::into_owned(
        exe.file_name()
            .expect(
                "owing to `cargo`'s stable formatting guarantees, if the program hasn't already \
                 thrown an error because of the lack of the `executable` key in the generated \
                 build json, then surely it has produced a file path with the last component being \
                 the name of the executable",
            )
            .to_string_lossy(),
    ))
}

fn run_tests<T: Display>(exe: String, tests: Vec<Test>, target_pkg: &T) -> Result<()> {
    tests.into_iter().try_fold((), |_, Test { num, out, err, rc, run, desc, .. }| {
        // Chanage the below constant if you ever start parsing more info
        // from the `Test` structure.
        const TEST_PARAMS: usize = 5;

        let [out, err, rc, run, desc] = [out, err, rc, run, desc]
            .into_iter()
            .enumerate()
            .try_fold([const { String::new() }; TEST_PARAMS], |mut accum, (idx, param)| {
                accum[idx] = param.ok_or_else(|| match idx {
                    | 0 => anyhow!("failed to find `out` test param"),
                    | 1 => anyhow!("failed to find `err` test param"),
                    | 2 => anyhow!("failed to find `rc` test param"),
                    | 3 => anyhow!("failed to find `run` test param"),
                    | 4 => anyhow!("failed to find `desc` test param"),
                    | _ => unimplemented!(
                        "if you hit this case, you most likely are considering more test \
                             parameters and should probably reevaluate the whole match arm and its \
                             surroundings"
                    ),
                })?;

                Ok(accum)
            })
            .with_context(|| {
                format!("failed while parsing test entry {num} for pkg: {target_pkg}")
            })?;
        let mut program_params = run.split_ascii_whitespace();
        let bin = program_params
            .next()
            .ok_or(anyhow!("failed to find binary name in `.rc` test file param"))
            .with_context(|| {
                format!("failed while parsing test entry {num} for pkg: {target_pkg}")
            })?;
        (bin.trim_matches(['.', '/']) == exe)
            .ok_or(anyhow!("crate binary doesn't match binary in `.run` test file param"))
            .with_context(|| {
                format!("crate binary: {exe}, binary name in `.run` test file param: {bin}")
            })?;
        println!("running test entry {num} for pkg {target_pkg}:\n{desc}");
        let output = Command::new(bin).args(program_params).output()?;
        let status = <ExitStatus as ExitStatusExt>::from_raw(
            rc.parse::<i32>()
                .context("failed to parse return code in `.rc` test file param")
                .with_context(|| {
                    format!("failed while parsing test entry {num} for pkg: {target_pkg}")
                })?,
        );
        let (real_status, real_out, real_err) = (
            output.status,
            String::from_utf8_lossy_owned(output.stdout),
            String::from_utf8_lossy_owned(output.stderr),
        );
        if status != real_status {
            return Err(anyhow!("test exit status doesn't match expected exit status"))
                .with_context(|| format!("test exit status: {}\nexpected: {status}", real_status));
        }
        if out != real_out {
            return Err(anyhow!("test stdout doesn't match expected stdout"))
                .with_context(|| format!("test stdout:\n{}\nexpected:\n{out}", real_out));
        }
        if err != real_err {
            return Err(anyhow!("test stderr doesn't match expected stderr"))
                .with_context(|| format!("test stderr:\n{}\nexpected:\n{err}", real_err));
        }

        Ok(())
    })?;
    println!("all tests passed");

    Ok(())
}
