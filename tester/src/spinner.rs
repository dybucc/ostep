use std::{
    borrow::Cow,
    fmt::{self, Display, Formatter},
    io::{self as std_io, Stdout as SyncStdout},
    sync::LazyLock,
};

use anyhow::Context;
use crossterm::{
    cursor::MoveToColumn,
    terminal::{Clear, ClearType},
};
use tokio::{
    io::{self, AsyncWriteExt, Stdout},
    sync::{
        Mutex,
        mpsc::{UnboundedReceiver, error::TryRecvError},
    },
    task,
};

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum SpinnerState {
    #[default]
    Hor,
    Left,
    Vert,
    Right,
}

impl SpinnerState {
    const PROGRESS_SPINNERS: [&str; 4] = ["-", "\\", "|", "/"];

    pub(crate) fn next(&mut self) {
        *self = match self {
            Self::Hor => Self::Left,
            Self::Left => Self::Vert,
            Self::Vert => Self::Right,
            Self::Right => Self::Hor,
        };
    }
}

impl Display for SpinnerState {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Hor => write!(f, "{}", Self::PROGRESS_SPINNERS[0]),
            Self::Left => write!(f, "{}", Self::PROGRESS_SPINNERS[1]),
            Self::Vert => write!(f, "{}", Self::PROGRESS_SPINNERS[2]),
            Self::Right => write!(f, "{}", Self::PROGRESS_SPINNERS[3]),
        }
    }
}

pub(crate) async fn spinner(mut rx: UnboundedReceiver<Cow<'static, str>>) -> anyhow::Result<()> {
    static SYNC_STDOUT: Mutex<LazyLock<SyncStdout>> =
        Mutex::const_new(LazyLock::new(std_io::stdout));

    async fn report(
        spinner_state: SpinnerState,
        msg: impl AsRef<str>,
        stdout: &mut Stdout,
    ) -> anyhow::Result<()> {
        let mut sync_stdout = SYNC_STDOUT.lock().await;

        task::spawn_blocking(move || {
            crossterm::execute!(sync_stdout, Clear(ClearType::CurrentLine), MoveToColumn(0))
        })
        .await
        .context("failed to manage task to clear line and move cursor")?
        .context("failed to clear line and move cursor")?;

        stdout
            .write_all(&format!("{} {}", spinner_state, msg.as_ref()).into_bytes())
            .await
            .context("failed to print out spinner state through async stdout")
    }

    let mut msg = None;
    let mut spinner = SpinnerState::default();

    let mut stdout = io::stdout();

    // NOTE: if there's a new message, it updates the message being output.
    // Otherwise, it simply reports the current progress message. This relies on the
    // fact the throughput of the channel can't stress this loop so much so as to
    // never print certain messages.
    loop {
        match rx.try_recv() {
            Ok(new_msg) => {
                spinner.next();
                msg = new_msg.into();
            }
            Err(TryRecvError::Disconnected) => break anyhow::Ok(()),
            Err(_) if msg.is_none() => (),
            Err(_) => report(spinner, msg.as_ref().unwrap(), &mut stdout)
                .await
                .context("failed while updating spinner messages")?,
        }
    }
}
