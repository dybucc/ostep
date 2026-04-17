#![feature(
    exit_status_error,
    string_from_utf8_lossy_owned,
    bool_to_result,
    control_flow_into_value
)]

extern crate self as tester;

use std::{
    borrow::{Borrow, Cow},
    env,
    ffi::{OsStr, OsString},
    fmt::Display,
    ops::ControlFlow,
    path::PathBuf,
    process::Command,
};

use anyhow::{Context, anyhow, bail};
use cargo_metadata::{MetadataCommand, Package};
use clap::Parser;
use crossterm::terminal;
use futures::{StreamExt as _, future};
use rayon::{
    iter::{IntoParallelIterator, ParallelIterator},
    slice::ParallelSliceMut,
};
use serde_json::Value;
use tester::{args::Args, spinner::spinner, test::Test};
use tokio::{
    fs,
    process::Command as AsyncCommand,
    sync::mpsc::{self, UnboundedSender},
    task::{self, JoinSet},
};
use tokio_stream::wrappers::ReadDirStream;

mod args;
mod spinner;
mod test;

const MAIN_RX_ERROR: &str = "rx end of main comms channel closed unexpectedly";

// FIXME: write a proc-macro that scans a function for its return value, checks
// if it's a `Result`, and if it is, then it parses its body and rewrites each
// fallible function callsite that is annotated with `?` (taking into account
// one ought parse anything that is not a closure or a macro) and rewrites it
// such that it goes from this:
// ```rust
// tokio::task::spawn_blocking(|| /* do something */).await?;
// ```
// To this:
// ```rust
// {
//     let res = tokio::task::spawn_blocking(|| /* do something */).await
//     if res.is_err() && crossterm::terminal::is_raw_mode_enabled() {
//         tokio::task::spawn_blocking(|| terminal::disable_raw_mode()).await.unwrap();
//         res?
//     };
// };
// ```
// This may require taking everything from `main` into a separate `inner_main`
// such that the rewrite isn't mixed up with `tokio`'s rewrite of
// `[tokio::main]`.

// FIXME(logger): there's somem printing statements that only run under
// `debug_assertions` which should either use some asynchronous `stderr`
// printing facility from `tokio`, or that should otherwise be replaced with a
// proper logger.

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    const INIT_ERR: &str = "failed to complete init terminal raw mode task";

    let (tx, rx) = mpsc::unbounded_channel();

    // Enable terminal raw mode to allow moving the cursor and rewriting progress
    // messages in a single line.
    task::spawn_blocking(terminal::enable_raw_mode)
        .await
        .context(INIT_ERR)?
        .context(INIT_ERR)?;

    // The futures reprsent the two main threads of execution here; Namely,
    // (1) the spinner that is always running in the "foreground" to notify of
    //     progress and/or errors, and
    // (2) the worker task that performs all of the testing harness functionality
    //     while occasionally updating the message shown on the spinner.
    match future::try_join(task::spawn(spinner(rx)), task::spawn(worker(tx))).await? {
        (Err(_), Err(_)) => bail!("fatal failure"),
        (Err(err), _) => Err(err).context("failed to handle spinner animation"),
        (_, res) => res,
    }
}

pub(crate) async fn worker(tx: UnboundedSender<Cow<'static, str>>) -> anyhow::Result<()> {
    let target_pkg = find_pkg(tx.clone()).await?;
    let mut tests = find_tests(tx.clone()).await?;

    let tests = task::spawn_blocking(move || {
        tests.par_sort_unstable_by_key(|&(_, test_num, _)| test_num);

        tests
    })
    .await
    .context("failed to handle blocking task to sort tests by entry number")?;

    let (exe, tests) = match future::try_join(copy_exe(&target_pkg), produce_tests(&tests)).await {
        Ok(inner) => inner,
        Err(err) => return Err(err),
    };

    run_tests(exe, tests, &target_pkg)
}

pub(crate) async fn find_pkg(tx: UnboundedSender<Cow<'static, str>>) -> anyhow::Result<String> {
    // NOTE: this gets used at the end once a matching package has been found, to
    // both set the pwd to that package's manifest path, and to return the package's
    // name.
    fn finalize(pkg: impl Borrow<Package>) -> anyhow::Result<String> {
        let Package {
            name,
            manifest_path,
            ..
        } = pkg.borrow();

        let pkg_path = manifest_path
            .parent()
            .ok_or(anyhow!("pkg manifest path doesn't have a parent dir"))
            .with_context(|| format!("failed while processing package: {}", *name))?;

        env::set_current_dir(pkg_path)
            .context("failed to set pwd during initialization")
            .with_context(|| format!("failed to set pwd to pkg manifest path: {pkg_path}"))?;

        Ok(name.to_string())
    }

    const ERR_MSG: &str = "failed during initialization";

    tx.send("parsing cargo workspace".into())
        .context(MAIN_RX_ERROR)
        .context(ERR_MSG)?;

    let workspace_metadata = MetadataCommand::parse(
        str::from_utf8(
            &AsyncCommand::new("cargo")
                .args(["metadata", "--format-version", "1", "--no-deps"])
                .output()
                .await
                .context("failed to query cargo workspace/package")
                .context(ERR_MSG)?
                .exit_ok()
                .context("`cargo metadata` invocation failed")
                .context(ERR_MSG)?
                .stdout,
        )
        .context("failed to convert `stdout` bytes from `cargo metadata` command to `str`")
        .context(ERR_MSG)?,
    )
    .context("failed to parse output of `cargo metadata`")
    .context(ERR_MSG)?;

    let workspace_packages = workspace_metadata.workspace_packages();

    let pwd = env::current_dir()
        .context("failed to fetch pwd")
        .context(ERR_MSG)?;

    // NOTE: if there's more than a single package in the working directory, we
    // check two things:
    // + Some workspace package's manifest path matches the pwd, in which case we
    //   default to that package.
    // + Some workspace package's manifest path matches the CLI argument that we got
    //   passed.
    //
    // Otherwise, either the workspace contains no packages or there's only one
    // package we can default to.
    Ok(match workspace_packages.len() {
        2.. => {
            let cli_pkg = Args::parse().package;
            let mut pkg = None;

            for workspace_pkg @ Package {
                name,
                manifest_path,
                ..
            } in &workspace_packages
            {
                if let Some(path) = manifest_path.parent()
                    && path == pwd
                {
                    pkg = Some(workspace_pkg);
                    break;
                }

                if let Some(cli_name) = &cli_pkg
                    && *cli_name == **name
                {
                    pkg = Some(workspace_pkg);
                    break;
                }
            }

            if let Some(pkg) = pkg {
                finalize(*pkg)?
            } else {
                bail!(
                    "cargo workspace package directory doesn't match pwd and no `-p` package \
                     option was provided"
                );
            }
        }
        1 => finalize(*workspace_packages.first().unwrap())?,
        _ => bail!("no packages found in current workspace: {}", pwd.display()),
    })
}

pub(crate) async fn find_tests(
    tx: UnboundedSender<Cow<'static, str>>,
) -> anyhow::Result<Vec<(PathBuf, usize, OsString)>> {
    tx.send("parsing tests".into()).context(MAIN_RX_ERROR)?;

    let mut dir_stream = ReadDirStream::new(
        fs::read_dir("./tests")
            .await
            .context("failed to read directory with tests")?,
    );

    let mut task_pool = JoinSet::new();

    // NOTE: this capacity is the one used later on to preallocate the vector that
    // will hold the test files. It only exists on a best-effort basis, but it
    // always guarantees that the vector will not have to allocate beyond this
    // capacity (i.e. it may overallocate for any directory entries under the
    // `tests` directory that are not tests themselves.)
    let mut speculative_cap = 0;

    while let Some(dir_entry) = dir_stream.next().await {
        match dir_entry {
            Ok(entry) => {
                speculative_cap += 1;

                // NOTE: we divide up the tasks here without performing the full processing that
                // is later on left to the traversal over the task pool because that traversal
                // only conditionally produces values (i.e. only produces values for files that
                // have been deemed to follow the preestablished schema for OSTEP tests.)
                task_pool.spawn(async move {
                    let path = entry.path();
                    let metadata = entry.metadata().await.with_context(|| {
                        format!(
                            "failed to read entry fs metadata when parsing `tests` directory \
                             entry: `{}`",
                            path.display()
                        )
                    })?;

                    anyhow::Ok((path, metadata))
                });
            }
            Err(io_err) => {
                task_pool.shutdown().await;

                return Err(anyhow!(io_err)).context(
                    "failed to read entry in the `tests` directory when parsing `tests` directory \
                     entries",
                );
            }
        }
    }

    let tasks_result = task_pool.join_all().await;

    // NOTE: the following performs a parallel fold of the above task results, such
    // that having put aside the operations that benefit from asynchronicity
    // (i.e. fetching file metadata,) the only thing left is to gather into a
    // single accumulator value all of the parsed tests, if they haven't failed
    // during such asyncrhonous I/O. We specifically gather 3-element tuples
    // consisting of the path to the test file, the entry number of the test and
    // the test type (which itself depends on the test path's file stem's
    // extension.)
    task::spawn_blocking(move || {
        tasks_result
            .into_par_iter()
            .try_fold(
                || Vec::with_capacity(speculative_cap),
                |mut accum, result| {
                    match result {
                        Ok((path, metadata)) => {
                            if metadata.is_file()
                                && let Some(entry_extension) = path.extension()
                                && matches!(
                                    entry_extension.as_encoded_bytes(),
                                    b"out" | b"err" | b"rc" | b"run" | b"desc" | b"pre" | b"post"
                                )
                            {
                                let num = path
                                    .file_stem()
                                    .ok_or(anyhow!("file doesn't contain file stem"))
                                    .context(
                                        "expected file stem to be a numeric value denoting the \
                                         test",
                                    )
                                    .with_context(|| {
                                        format!(
                                            "failed when parsing `tests` directory entry: `{}`",
                                            path.display()
                                        )
                                    })?
                                    .to_str()
                                    .ok_or(anyhow!("file contains non-utf8 codepoints"))
                                    .context(
                                        "expected utf-8-compliant values for each test; each test \
                                         should denote a numeric value",
                                    )
                                    .with_context(|| {
                                        format!(
                                            "failed when parsing `tests` directory entry: `{}`",
                                            path.display()
                                        )
                                    })?
                                    .parse::<usize>()
                                    .context("expected file to denote a test number in the suite")
                                    .with_context(|| {
                                        format!(
                                            "failed when parsing `tests` directory entry: `{}`",
                                            path.display()
                                        )
                                    })?;

                                let extension = entry_extension.to_os_string();

                                accum.push((path, num, extension));
                            }
                        }
                        Err(err) => return Err(err),
                    }

                    Ok(accum)
                },
            )
            .try_reduce(Vec::new, |mut a, mut b| {
                a.append(&mut b);

                Ok(a)
            })
    })
    .await
    .context("failed to handle task to manage test entry parsing")?
}

// Things that could be made async here:
// - Each test entry's files are read in serailly, which is to some extent a
//   blocking operation driven by a CPU-bound operation (traversing all test
//   entries.) That could be made async, but the potential gains here are making
//   the overall function async to allow another routine (`copy_exe()`) to run
//   concurrencly.
#[expect(clippy::unused_async, reason = "wip.")]
pub(crate) async fn produce_tests(
    tests: &[(PathBuf, usize, OsString)],
) -> anyhow::Result<Vec<Test>> {
    anyhow::Ok(
        tests
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
                            let printable_path = test_path.display();

                            Some(
                                std::fs::read_to_string($test.canonicalize().with_context(
                                    || {
                                        format!(
                                            "failed while parsing `tests` directory entry `{}`",
                                            printable_path
                                        )
                                    },
                                )?)
                                .with_context(|| {
                                    format!(
                                        "failed while parsing `tests` directory entry `{}`",
                                        printable_path
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
                        _ => unreachable!(
                            "all file extensions have been filtered past `find_tests()`"
                        ),
                    }

                    anyhow::Ok((tests, current_test))
                },
            )?
            .0,
    )
}

pub(crate) fn preprocess_build_json(input: Vec<u8>) -> String {
    let len = input.len();

    let out = input
        .into_par_iter()
        .try_fold(
            || String::with_capacity(len),
            |mut output, b| {
                if b != b'\n' {
                    output.push(char::from(b));
                    return ControlFlow::Continue(output);
                }

                ControlFlow::Break(output)
            },
        )
        .try_reduce(String::new, |a, b| {
            let mut out = a.into_bytes();
            out.append(&mut b.into_bytes());

            ControlFlow::Continue(String::from_utf8_lossy_owned(out))
        })
        .into_value();

    #[cfg(debug_assertions)]
    eprintln!("{out}");

    out
}

// TODO: finish verifying the below routine's compliance with non-blocking
// operations in the `tokio` runtime.

pub(crate) async fn copy_exe<T: Display>(target_pkg: &T) -> anyhow::Result<String> {
    let cmd_stdout =
        AsyncCommand::new("cargo")
            .args(["build", "--release", "--message-format=json"])
            .output()
            .await
            .context("failed to spawn `cargo-build` on pwd")
            .with_context(|| {
                format!(
                    "failed while trying to build binary for cargo workspace package: {target_pkg}",
                )
            })?
            .exit_ok()
            .context("package compilation through `cargo-build` failed")
            .with_context(|| {
                format!(
                    "failed while trying to build binary for cargo workspace package: {target_pkg}",
                )
            })?
            .stdout;

    // FIXME(logger):
    #[cfg(debug_assertions)]
    eprintln!(
        "copy_exe: {}",
        String::from_utf8_lossy_owned(cmd_stdout.clone())
    );

    let preprocessed_json = task::spawn_blocking(move || preprocess_build_json(cmd_stdout))
        .await
        .context("failed to handle task managing cargo build json parsing")?;

    let exe = if let Value::Object(map) =
        serde_json::from_str(&preprocessed_json)
            .context("failed to parse json output from `cargo-build`")
            .with_context(|| {
                format!(
                    "failed while trying to build binary for cargo workspace package: {target_pkg}",
                )
            })?
        && let Some(Value::String(s)) = map.get("executable")
    {
        Some(PathBuf::from(s))
    } else {
        None
    }
    .ok_or(anyhow!(
        "failed to find `executable` entry in cargo build json output"
    ))
    .with_context(|| {
        format!("failed while trying to build binary for cargo workspace package: {target_pkg}")
    })?;

    AsyncCommand::new("cp")
        .args([exe.as_os_str(), OsStr::new(".")])
        .status()
        .await
        .context("failed to copy binary executable to pwd")
        .with_context(|| {
            format!("failed while managing binary for cargo workspace package: {target_pkg}")
        })?;

    anyhow::Ok(Cow::into_owned(
        exe.file_name()
            .expect(
                "owing to `cargo`'s stable formatting guarantees, if the program hasn't already \
                 thrown an error because the `executable` key in the generated build json is \
                 missing, then surely it has produced a file path with the last component being \
                 the name of the executable",
            )
            .to_string_lossy(),
    ))
}

// Things that could be made async here:
// - Each test run is bound to block with the invocation command for the program
//   being tested, so that could be made async.
fn run_tests<T: Display>(exe: String, tests: Vec<Test>, target_pkg: &T) -> anyhow::Result<()> {
    tests.into_iter().try_for_each(
        |Test {
             num,
             out,
             err,
             rc,
             run,
             desc,
             ..
         }| {
            // Chanage the below constant if you ever start parsing more info from the
            // `Test` structure.
            const TEST_PARAMS: usize = 5;

            let [out, err, rc, run, desc] = [out, err, rc, run, desc]
                .into_iter()
                .enumerate()
                .try_fold(
                    [const { String::new() }; TEST_PARAMS],
                    |mut accum, (idx, param)| {
                        accum[idx] = param.ok_or_else(|| match idx {
                            0 => anyhow!("failed to find `out` test param"),
                            1 => anyhow!("failed to find `err` test param"),
                            2 => anyhow!("failed to find `rc` test param"),
                            3 => anyhow!("failed to find `run` test param"),
                            4 => anyhow!("failed to find `desc` test param"),
                            _ => unimplemented!(
                                "if you hit this case, you are most likely considering more test \
                                 parameters and should probably reevaluate the whole match arm \
                                 and its surroundings"
                            ),
                        })?;

                        anyhow::Ok(accum)
                    },
                )
                .with_context(|| {
                    format!("failed while parsing test entry {num} for pkg: {target_pkg}")
                })?;
            #[cfg(debug_assertions)]
            eprintln!(
                "pkg entry: {num}\nout: {out}\nerr: {err}\nrc: {rc}\nrun: {run}\ndesc: {desc}"
            );
            let mut program_params = run.split_ascii_whitespace();
            let bin = program_params
                .next()
                .ok_or(anyhow!(
                    "failed to find binary name in `.rc` test file param"
                ))
                .with_context(|| {
                    format!("failed while parsing test entry {num} for pkg: {target_pkg}")
                })?;
            (bin.trim_matches(['.', '/']) == exe)
                .ok_or(anyhow!(
                    "crate binary doesn't match binary in `.run` test file param"
                ))
                .with_context(|| {
                    format!("crate binary: {exe}, binary name in `.run` test file param: {bin}")
                })?;
            print!("running test entry {num} for pkg {target_pkg}:\n{desc}");
            let output = Command::new(bin).args(program_params).output()?;
            let status = rc
                .trim()
                .parse::<i32>()
                .context("failed to parse return code in `.rc` test file param")
                .with_context(|| {
                    format!("failed while parsing test entry {num} for pkg: {target_pkg}")
                })?;
            let (real_status, real_out, real_err) = (
                output
                    .status
                    .code()
                    .ok_or_else(|| {
                        anyhow!("failed to parse wait status of tested binary program as exit code")
                    })
                    .with_context(|| {
                        format!("failed while parsing test entry {num} for pkg: {target_pkg}")
                    })?,
                String::from_utf8_lossy_owned(output.stdout),
                String::from_utf8_lossy_owned(output.stderr),
            );
            (status == real_status)
                .ok_or(anyhow!(
                    "test exit status doesn't match expected exit status"
                ))
                .with_context(|| {
                    format!("\ntest exit status: {real_status}\nexpected: {status}")
                })?;
            (out == real_out)
                .ok_or(anyhow!("test stdout doesn't match expected stdout"))
                .with_context(|| format!("\ntest stdout:\n{real_out}\nexpected:\n{out}"))?;
            (err == real_err)
                .ok_or(anyhow!("test stderr doesn't match expected stderr"))
                .with_context(|| format!("\ntest stderr:\n{real_err}\nexpected:\n{err}"))?;
            println!("sucess\n---");

            anyhow::Ok(())
        },
    )?;
    println!("all tests passed");
    cleanup(exe)?;

    anyhow::Ok(())
}

fn cleanup(mut exe: String) -> anyhow::Result<()> {
    exe.insert_str(0, "./");
    Command::new("rm")
        .arg(exe)
        .status()
        .context("failed to invoke clean up command")?
        .exit_ok()
        .context("failed to clean up testing resources")?;

    anyhow::Ok(())
}
