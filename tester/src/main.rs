#![feature(
    exit_status_error,
    string_from_utf8_lossy_owned,
    bool_to_result,
    control_flow_into_value
)]

use std::{
    borrow::Cow,
    env,
    ffi::{OsStr, OsString},
    fmt::Display,
    hint, iter,
    ops::ControlFlow,
    path::PathBuf,
    process::Command,
    result::Result,
};

use anyhow::{Context, Ok, anyhow, bail};
use cargo_metadata::MetadataCommand;
use clap::Parser;
use futures::{StreamExt as _, TryStreamExt, future, stream};
use serde_json::Value;
use tokio::{
    fs,
    io::{self, AsyncWriteExt},
    process::Command as AsyncCommand,
    sync::mpsc::{self, UnboundedReceiver, UnboundedSender, error::TryRecvError},
    task,
};
use tokio_stream::wrappers::ReadDirStream;

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
    disable_help_flag = true,
    disable_help_subcommand = true,
    infer_long_args = true
)]
struct Args {
    /// Specify the package to work on in a multi-package workspace.
    #[arg(short, long)]
    package: Option<String>,
}

#[derive(Debug, Default)]
pub(crate) enum SpinnerState {
    #[default]
    Hor,
    Left,
    Vert,
    Right,
}

impl SpinnerState {
    const PROGRESS_SPINNERS: [&str; 4] = ["-", "\\", "|", "/"];

    fn next(self) -> Self {
        match self {
            Self::Hor => Self::Left,
            Self::Left => Self::Vert,
            Self::Vert => Self::Right,
            Self::Right => Self::Hor,
        }
    }

    fn state(&self) -> &'static str {
        match self {
            Self::Hor => Self::PROGRESS_SPINNERS[0],
            Self::Left => Self::PROGRESS_SPINNERS[1],
            Self::Vert => Self::PROGRESS_SPINNERS[2],
            Self::Right => Self::PROGRESS_SPINNERS[3],
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let comms = mpsc::unbounded_channel();
    let tx = comms.0;
    let rx = comms.1;
    let spinner_task = {
        let spinner = spinner(rx);
        task::spawn(spinner)
    };
    let worker_task = {
        let work = async move {
            // FIXME: remove the below assignment once everything is async and replace the
            // last clone in the async block with an owning (moved) `tx`.
            let tx = tx;
            let target_pkg = {
                let tx = tx.clone();
                let task = find_pkg(tx);
                let res = task.await;
                res?
            };
            let mut tests = {
                let tx = tx.clone();
                let task = find_tests(tx);
                let res = task.await;
                res?
            };
            let test_num_key = |input: &(PathBuf, usize, OsString)| input.1;
            tests.sort_unstable_by_key(test_num_key);
            let (exe, tests) = (copy_exe(&target_pkg)?, produce_tests(&tests)?);

            Ok((exe, tests, target_pkg))
        };
        task::spawn(work)
    };
    let res = {
        let joiner = future::try_join(spinner_task, worker_task);
        let join_res = joiner.await;
        let res = join_res?;
        let spinner_res = res.0;
        let worker_res = res.1;
        if spinner_res.is_err() {
            bail!("failed to handle spinner animation");
        }
        if worker_res.is_err() {
            bail!("failed to complete testing");
        }
        worker_res?
    };
    let exe = res.0;
    let tests = res.1;
    let target_pkg = res.2;
    run_tests(exe, tests, &target_pkg)
}

const RX_ERROR: &str = "rx end of main comms channel closed unexpectedly";

#[macro_export]
macro_rules! awrite {
    ($buf:expr) => {{
        use ::tokio::io;

        io::stdout().write_all($buf).await
    }};
}

// FIXME: refactor the `spinner` routine to use raw terminal printing
// capabilities, as otherwise the report messages are printed one after the
// other. This will likely require further reworks when it comes to stdout
// routine calls in other routines.

pub(crate) async fn spinner(mut rx: UnboundedReceiver<Cow<'static, str>>) -> anyhow::Result<()> {
    macro_rules! report {
        ($msg:expr, $spinner:expr) => {{
            use ::std::format;

            #[expect(unused, reason = "It's actually used right below.")]
            use crate::awrite;

            let msg = format!("{} {}\n", $spinner.state(), $msg).into_bytes();
            let msg = &msg;
            awrite!(msg)
        }};
    }

    let comms = mpsc::channel(1);
    let inner_tx = comms.0;
    let mut inner_rx = comms.1;
    let msg_intercept_task = {
        let msg_intercept = async move {
            loop {
                let event = rx.recv();
                let maybe_msg = event.await;
                if let Some(msg) = maybe_msg {
                    let report_to_spinner = inner_tx.send(msg);
                    report_to_spinner.await;
                }
            }
        };
        task::spawn(msg_intercept)
    };
    let spinner_task = {
        let spinner = async move {
            let mut msg = None;
            let mut spinner = SpinnerState::default();
            loop {
                let event = inner_rx.try_recv();
                match event {
                    Result::Ok(new_msg) => {
                        let new_msg = Some(new_msg);
                        msg = new_msg;
                        let next_spinner = spinner.next();
                        spinner = next_spinner;
                    }
                    Result::Err(err_kind) => {
                        let channel_done = matches!(err_kind, TryRecvError::Disconnected);
                        if channel_done {
                            break;
                        }
                        let first_failure = msg.is_none();
                        if !first_failure {
                            let msg = {
                                let msg = msg.as_ref();
                                msg.unwrap()
                            };
                            let res = {
                                let res = report!(msg, spinner);
                                res.context("failed to write to stdout spinner report messages")
                            };
                            res?;
                        }
                    }
                }
            }
            Ok(())
        };
        task::spawn(spinner)
    };
    let join_res = {
        let task_joiner = future::try_join(msg_intercept_task, spinner_task);
        task_joiner.await
    };
    join_res?;
    Ok(())
}

pub(crate) async fn find_pkg(tx: UnboundedSender<Cow<'static, str>>) -> anyhow::Result<String> {
    const INIT_MSG: &str = "failed during initialization";

    tx.send("parsing cargo workspace".into())
        .context(RX_ERROR)
        .context(INIT_MSG)?;
    let workspace_metadata = MetadataCommand::parse(
        str::from_utf8(
            &AsyncCommand::new("cargo")
                .args(["metadata", "--format-version", "1", "--no-deps"])
                .output()
                .await
                .context("failed to query cargo workspace/package")
                .context(INIT_MSG)?
                .exit_ok()
                .context("`cargo metadata` invocation failed")
                .context(INIT_MSG)?
                .stdout,
        )
        .context("failed to convert `stdout` bytes from `cargo metadata` command to `str`")
        .context(INIT_MSG)?,
    )
    .context("failed to parse output of `cargo metadata`")
    .context(INIT_MSG)?;
    let workspace_packages = workspace_metadata.workspace_packages();

    Ok(match workspace_packages.len() {
        2.. => {
            let pwd = env::current_dir().context("failed to fetch pwd during initialization")?;
            let arg_pkg_name = Args::parse().package;

            if let ControlFlow::Break(pkg) = workspace_packages.iter().try_fold((), |(), pkg| {
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

                ControlFlow::Continue(())
            }) {
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
        }
        1 => {
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
        }
        _ => bail!("no packages found"),
    })
}

pub(crate) async fn find_tests(
    tx: UnboundedSender<Cow<'static, str>>,
) -> anyhow::Result<Vec<(PathBuf, usize, OsString)>> {
    tx.send("parsing tests".into()).context(RX_ERROR)?;

    ReadDirStream::new(fs::read_dir("./tests").await.context(
        "the `tests` directory should be present in the folder where you're running the binary",
    )?)
    .try_fold(Vec::new(), |mut accum, entry| {
        let entry = entry.context(
            "failed to read entry in the `tests` directory when parsing `tests` directory entries",
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

// Things that could be made async here:
// - Each test entry's files are read in serailly, which is to some extent a
//   blocking operation driven by a CPU-bound operation (traversing all test
//   entries.) That could be made async, but the potential gains here are making
//   the overall function async to allow another routine (`copy_exe()`) to run
//   concurrencly.
fn produce_tests(tests: &[(PathBuf, usize, OsString)]) -> anyhow::Result<Vec<Test>> {
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
                        let printable_path = test_path.display();

                        Some(
                            fs::read_to_string($test.canonicalize().with_context(|| {
                                format!(
                                    "failed while parsing `tests` directory entry `{}`",
                                    printable_path
                                )
                            })?)
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
                    _ => unreachable!("all file extensions have been filtered past `find_tests()`"),
                }

                Ok((tests, current_test))
            },
        )?
        .0)
}

fn preprocess_build_json(input: Vec<u8>) -> String {
    let len = input.len();

    cfg_select! {
      debug_assertions => {
        let out = input
          .into_iter()
          .try_fold(String::with_capacity(len), |mut output, b| {
            if b != b'\n' {
              output.push(char::from(b));
              return ControlFlow::Continue(output);
            }

            ControlFlow::Break(output)
          })
          .into_value();
        eprintln!("{out}");

        out
      }
      _ => {
        input
          .into_iter()
          .try_fold(String::with_capacity(len), |mut output, b| {
            if b != b'\n' {
              output.push(char::from(b));
              return ControlFlow::Continue(output);
            }

            ControlFlow::Break(output)
          })
          .into_value()
      }
    }
}

fn parse_build_json(input: Value) -> Option<PathBuf> {
    if let Value::Object(map) = input
        && let Some(Value::String(s)) = map.get("executable")
    {
        Some(PathBuf::from(s))
    } else {
        None
    }
}

// Things that could be made async here:
// - Nothing much. The only blocking operations are command invocations that
//   rely on being executed in serial, and whose output is required for the
//   final `run_tests()` operation. It could be made async "overall," because
//   there's other I/O-bound operations in other routines that could be running
//   as well, so there's that.
fn copy_exe<T: Display>(target_pkg: &T) -> anyhow::Result<String> {
    let exe =
        parse_build_json(
            serde_json::from_str(&preprocess_build_json({
                let input = Command::new("cargo")
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
                    .stdout;
                #[cfg(debug_assertions)]
                eprintln!("{}", String::from_utf8_lossy_owned(input.clone()));

                input
            }))
            .context("failed to parse json output from `cargo-build`")
            .with_context(|| {
                format!(
                    "failed while trying to build binary for cargo workspace package: {target_pkg}",
                )
            })?,
        )
        .ok_or(anyhow!(
            "failed to find `executable` entry in cargo build json output"
        ))
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

                        Ok(accum)
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

            Ok(())
        },
    )?;
    println!("all tests passed");
    cleanup(exe)?;

    Ok(())
}

fn cleanup(mut exe: String) -> anyhow::Result<()> {
    exe.insert_str(0, "./");
    Command::new("rm")
        .arg(exe)
        .status()
        .context("failed to invoke clean up command")?
        .exit_ok()
        .context("failed to clean up testing resources")?;

    Ok(())
}
