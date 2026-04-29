#![feature(
    exit_status_error,
    string_from_utf8_lossy_owned,
    bool_to_result,
    control_flow_into_value,
    bstr
)]

extern crate self as tester;

use std::{
    borrow::{Borrow, Cow},
    bstr::ByteStr,
    env,
    ffi::{OsStr, OsString},
    fmt::Display,
    fs::Metadata,
    ops::ControlFlow,
    path::PathBuf,
};

use anyhow::{Context, anyhow, bail};
use cargo_metadata::{MetadataCommand, Package};
use clap::Parser;
use crossterm::terminal;
use futures::{StreamExt, future};
use rayon::{
    iter::{IntoParallelIterator, ParallelIterator},
    slice::ParallelSliceMut,
};
use serde_json::Value;
use tester::{args::Args, spinner::spinner, test::Test};
use tester_impl::defer_drm;
use tokio::{
    fs,
    process::Command,
    sync::mpsc::{self, UnboundedSender},
    task::{self, JoinSet},
};
use tokio_stream::wrappers::ReadDirStream;

mod args;
mod spinner;
mod test;

const MAIN_RX_ERROR: &str = "rx end of main comms channel closed unexpectedly";

// FIXME(logger): there's some printing statements that only run under
// `debug_assertions` which should either use some asynchronous `stderr` from
// `tokio`, or otherwise should be replaced with a proper logger.

#[tracing::instrument(skip_all)]
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    const INIT_ERR: &str = "failed to complete init terminal raw mode task";

    let (tx, rx) = mpsc::unbounded_channel();

    task::spawn_blocking(terminal::enable_raw_mode)
        .await
        .context(INIT_ERR)?
        .context(INIT_ERR)?;

    // The futures represent the two main threads of execution here; Namely,
    // (1) the spinner that is always running in the "foreground" to notify of
    //     progress and/or errors, and
    // (2) the worker task that performs all of the testing harness functionality
    //     while occasionally updating the message shown on the spinner.
    match future::try_join(task::spawn(spinner(rx)), task::spawn(worker(tx))).await? {
        (err @ Err(_), _) => err.context("failed to handle spinner animation"),
        (_, res) => res,
    }
}

#[tracing::instrument(skip_all)]
#[defer_drm]
pub(crate) async fn worker(tx: UnboundedSender<Cow<'static, str>>) -> anyhow::Result<()> {
    let target_pkg = find_pkg(tx.clone()).await?;
    let mut tests = find_tests(tx.clone()).await?;

    // NOTE: because the below two tasks run concurrently, we prefer not to have
    // them intersperse output so for now we only report the tasks that are about to
    // be made.
    tx.send("copying executable to `tests/` and processing tests".into())
        .context(MAIN_RX_ERROR)?;

    let (tests, exe) = match future::try_join(
        task::spawn_blocking(move || {
            tests.par_sort_unstable_by_key(|&(_, test_num, _)| test_num);

            tests
        }),
        task::spawn(copy_exe(target_pkg.clone())),
    )
    .await
    {
        Ok((tests, Ok(exe))) => (tests, exe),
        Ok((_, Err(copy_err))) => bail!(copy_err),
        Err(task_err) => {
            return Err(task_err)
                .context("failed to handle blocking task to sort tests by entry number");
        }
    };

    let tests = produce_tests(tests).await?;

    tx.send("running tests".into()).context(MAIN_RX_ERROR)?;

    run_tests(exe, tests, target_pkg).await
}

#[tracing::instrument(skip_all)]
#[defer_drm]
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
            &Command::new("cargo")
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
    match workspace_packages.len() {
        2.. => {
            // NOTE: if we got issued a package name, we ought prioritize finding a package
            // of that name, before attempting to fall back to the package that we may be at
            // in the current working directory.
            if let Some(pkg) = if let Some(cli_name) = Args::parse().package {
                workspace_packages
                    .iter()
                    .try_fold(
                        None,
                        |mut fallback_pwd,
                         pkg @ Package {
                             name,
                             manifest_path,
                             ..
                         }| {
                            if cli_name == **name {
                                return ControlFlow::Break(pkg);
                            }

                            if *manifest_path == pwd {
                                fallback_pwd = pkg.into();
                            }

                            ControlFlow::Continue(fallback_pwd)
                        },
                    )
                    .map_break(Some)
                    .into_value()
            } else {
                workspace_packages
                    .iter()
                    .find(|Package { manifest_path, .. }| *manifest_path == pwd)
            } {
                finalize(*pkg)
            } else {
                bail!(
                    "cargo workspace package directory doesn't match pwd and no `-p` package \
                     option was provided"
                );
            }
        }
        1 => finalize(*workspace_packages.first().unwrap()),
        _ => bail!("no packages found in current workspace: {}", pwd.display()),
    }
}

#[tracing::instrument(skip_all)]
#[defer_drm]
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
    // that having put aside the operations that benefit from asynchronicity (i.e.
    // fetching file metadata,) the only thing left is to gather into a single
    // accumulator value all of the parsed tests, if they haven't failed during such
    // asynchronous I/O. We specifically gather 3-element tuples consisting of the
    // path to the test file, the entry number of the test and the test type (which
    // itself depends on the test path's file stem's extension.)
    task::spawn_blocking(move || proc_tests(tasks_result, speculative_cap))
        .await
        .context("failed to handle task to manage test entry parsing")?
}

#[tracing::instrument(skip_all)]
pub(crate) fn proc_tests(
    tasks_result: Vec<anyhow::Result<(PathBuf, Metadata)>>,
    init_cap: usize,
) -> anyhow::Result<Vec<(PathBuf, usize, OsString)>> {
    tasks_result
        .into_par_iter()
        .try_fold(
            || Vec::with_capacity(init_cap),
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
                                    "expected file stem to be a numeric value denoting the test",
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
}

#[tracing::instrument(skip_all)]
#[defer_drm]
pub(crate) async fn produce_tests(
    tests: Vec<(PathBuf, usize, OsString)>,
) -> anyhow::Result<Vec<Test>> {
    let mut task_pool = JoinSet::new();

    let num_tests = tests.len();

    for (test_path, _, test_extension) in tests {
        task_pool.spawn(async move {
            let mut test = Test::default();

            macro_rules! check_entry {
                ($test:expr) => {{
                    let ctx_msg = || {
                        format!(
                            "failed while parsing `tests` directory entry `{}`",
                            $test.display()
                        )
                    };

                    fs::read_to_string(fs::canonicalize(&$test).await.with_context(ctx_msg)?)
                        .await
                        .with_context(ctx_msg)?
                        .into()
                }};
            }

            match test_extension.as_encoded_bytes() {
                b"rc" => test.rc = check_entry!(test_path),
                b"out" => test.out = check_entry!(test_path),
                b"err" => test.err = check_entry!(test_path),
                b"run" => test.run = check_entry!(test_path),
                b"desc" => test.desc = check_entry!(test_path),
                b"pre" => test.pre = check_entry!(test_path),
                b"post" => test.post = check_entry!(test_path),
                _ => {
                    unreachable!("all other file extensions have been filtered past `find_tests()`")
                }
            }

            anyhow::Ok(test)
        });
    }

    let mut out = Vec::with_capacity(num_tests);

    while let Some(res) = task_pool.join_next().await {
        out.push(res.context("failed to handle task management while reading test files")??);
    }

    Ok(out)
}

#[tracing::instrument(skip_all)]
pub(crate) fn preprocess_build_json(input: Vec<u8>) -> String {
    let len = input.len();

    let out = input
        .into_par_iter()
        .try_fold(
            || String::with_capacity(len),
            |mut output, b| {
                if b == b'\n' {
                    return ControlFlow::Break(output);
                }

                output.push(char::from(b));

                ControlFlow::Continue(output)
            },
        )
        .try_reduce(String::new, |mut a, b| {
            a.push_str(&b);

            ControlFlow::Continue(a)
        })
        .into_value();

    // FIXME(logger):
    #[cfg(debug_assertions)]
    eprintln!("{out}");

    out
}

#[tracing::instrument(skip_all)]
#[defer_drm]
pub(crate) async fn copy_exe<T: Display>(target_pkg: T) -> anyhow::Result<String> {
    let ctx_msg =
        || format!("failed while trying to build binary for cargo workspace package: {target_pkg}");

    let cmd_stdout = Command::new("cargo")
        .args(["build", "--release", "--message-format=json"])
        .output()
        .await
        .context("failed to spawn `cargo-build` on pwd")
        .with_context(ctx_msg)?
        .exit_ok()
        .context("package build through `cargo-build` failed")
        .with_context(ctx_msg)?
        .stdout;

    // FIXME(logger):
    #[cfg(debug_assertions)]
    eprintln!(
        "copy_exe: {}",
        String::from_utf8_lossy_owned(cmd_stdout.clone())
    );

    // NOTE: we don't encode the `executable` key of the JSON in a type nor constant
    // because it's part of the stability guarantees in cargo's output.
    let exe = if let Value::Object(map) = serde_json::from_str(
        &task::spawn_blocking(move || preprocess_build_json(cmd_stdout))
            .await
            .context("failed to handle task managing cargo build json parsing")?,
    )
    .context("failed to parse json output from `cargo-build`")
    .with_context(ctx_msg)?
        && let Some(Value::String(exe)) = map.get("executable")
    {
        Ok(PathBuf::from(exe))
    } else {
        Err(anyhow!(
            "failed to find `executable` entry in cargo build json output"
        ))
    }
    .with_context(ctx_msg)?;

    Command::new("cp")
        .args([exe.as_os_str(), OsStr::new(".")])
        .status()
        .await
        .context("failed to copy binary executable to pwd")
        .with_context(ctx_msg)?;

    Ok(Cow::into_owned(
        exe.file_name()
            .expect(
                "file stem should be present if cargo hasn't failed in producing the build \
                 metadata output",
            )
            .to_string_lossy(),
    ))
}

#[tracing::instrument(skip_all)]
#[defer_drm]
pub(crate) async fn run_tests<T>(exe: String, tests: Vec<Test>, target_pkg: T) -> anyhow::Result<()>
where
    for<'a> T: 'a + Display + Send + Sync + Clone,
{
    // FIXME(refactor): change the below constant if you ever start parsing more
    // info from the `Test` structure.
    const TEST_PARAMS: usize = 4;

    let mut task_pool = tests.into_iter().fold(
        JoinSet::new(),
        |mut task_pool,
         Test {
             num,
             out,
             err,
             rc,
             run,
             ..
         }| {
            let exe = exe.clone();
            let target_pkg = target_pkg.clone();

            task_pool.spawn(async move {
                let ctx_msg =
                    || format!("failed while parsing test entry {num} for pkg: {target_pkg}");

                let [out, err, rc, run] = [out, err, rc, run]
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
                                _ => unimplemented!(
                                    "the test harness is broken, report to the issue tracker \
                                     about the pattern matching at the start of `run_tests()`"
                                ),
                            })?;

                            anyhow::Ok(accum)
                        },
                    )
                    .with_context(ctx_msg)?;

                let mut program_params = run.split_ascii_whitespace();
                let bin = program_params
                    .next()
                    .ok_or(anyhow!(
                        "failed to find binary name in `.rc` test file param"
                    ))
                    .with_context(ctx_msg)?;

                (bin.trim_start_matches(['.', '/']) == exe)
                    .ok_or(anyhow!(
                        "crate binary doesn't match binary in `.run` test file param"
                    ))
                    .with_context(|| {
                        format!("crate binary: {exe}, binary name in `.run` test file param: {bin}")
                    })?;

                let output = Command::new(bin).args(program_params).output().await?;
                let status = rc
                    .trim()
                    .parse::<i32>()
                    .context("failed to parse return code in `.rc` test file param")
                    .with_context(ctx_msg)?;

                let (real_status, real_out, real_err) = (
                    output
                        .status
                        .code()
                        .ok_or_else(|| {
                            anyhow!(
                                "failed to parse wait status of tested binary program as exit code"
                            )
                        })
                        .with_context(ctx_msg)?,
                    ByteStr::new(&output.stdout),
                    ByteStr::new(&output.stderr),
                );

                (status == real_status)
                    .ok_or(anyhow!(
                        "test exit status doesn't match expected exit status"
                    ))
                    .with_context(|| {
                        format!("\ntest exit status: {real_status}\nexpected: {status}")
                    })?;

                (out.as_bytes() == real_out)
                    .ok_or(anyhow!("test stdout doesn't match expected stdout"))
                    .with_context(|| format!("\ntest stdout:\n{real_out}\nexpected:\n{out}"))?;

                (err.as_bytes() == real_err)
                    .ok_or(anyhow!("test stderr doesn't match expected stderr"))
                    .with_context(|| format!("\ntest stderr:\n{real_err}\nexpected:\n{err}"))?;

                anyhow::Ok(())
            });

            task_pool
        },
    );

    while let Some(res) = task_pool.join_next().await {
        // NOTE: we don't provide context to the inner result because that one already
        // holds the error we added context to inside each task (none in particular,
        // whichever one of them all.)
        res.context("failed to handle some task while running tests")??;
    }

    cleanup(exe).await?;

    Ok(())
}

#[defer_drm]
pub(crate) async fn cleanup(mut exe: String) -> anyhow::Result<()> {
    exe.insert_str(0, "./");

    Command::new("rm")
        .arg(exe)
        .status()
        .await
        .context("failed to invoke clean up command")?
        .exit_ok()
        .context("failed to clean up testing resources")?;

    anyhow::Ok(())
}
