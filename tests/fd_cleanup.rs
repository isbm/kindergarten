#![cfg(unix)]

use kindergarten::Kindergarten;
use std::{
    io,
    process::Command as StdCommand,
    sync::{Arc, LazyLock, Mutex},
    time::Duration,
};
use tokio::{
    process::Command,
    task::JoinSet,
    time::{sleep, timeout},
};

static TEST_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

const ASSERT_TIMEOUT: Duration = Duration::from_secs(5);
const ASSERT_POLL_INTERVAL: Duration = Duration::from_millis(20);

fn quick_exit_command() -> Command {
    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg("exit 0");
    cmd
}

fn delayed_exit_command() -> Command {
    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg("sleep 0.05");
    cmd
}

fn long_lived_command() -> Command {
    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg("sleep 30");
    cmd
}

fn lsof_pipe_fd_count(pid: u32) -> io::Result<Option<usize>> {
    let output = match StdCommand::new("lsof")
        .arg("-p")
        .arg(pid.to_string())
        .arg("-Fn")
        .output()
    {
        Ok(output) => output,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    };

    if !output.status.success() {
        return Err(io::Error::other(format!(
            "lsof failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(Some(stdout.lines().filter(|line| *line == "npipe").count()))
}

fn lsof_pipe_snapshot(pid: u32) -> io::Result<Option<String>> {
    let output = match StdCommand::new("lsof")
        .arg("-p")
        .arg(pid.to_string())
        .output()
    {
        Ok(output) => output,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    };

    if !output.status.success() {
        return Err(io::Error::other(format!(
            "lsof failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }

    Ok(Some(String::from_utf8_lossy(&output.stdout).into_owned()))
}

fn current_pipe_fd_count() -> io::Result<Option<usize>> {
    lsof_pipe_fd_count(std::process::id())
}

async fn assert_pipe_fds_eventually(expected: usize, context: &str) -> io::Result<()> {
    let pid = std::process::id();
    let wait_for_match = async {
        loop {
            let current = lsof_pipe_fd_count(pid)?.expect("`lsof` disappeared during the test");
            if current == expected {
                return Ok::<(), io::Error>(());
            }
            sleep(ASSERT_POLL_INTERVAL).await;
        }
    };

    match timeout(ASSERT_TIMEOUT, wait_for_match).await {
        Ok(result) => result,
        Err(_) => {
            let current = lsof_pipe_fd_count(pid)?.expect("`lsof` disappeared during the test");
            let snapshot = lsof_pipe_snapshot(pid)?.unwrap_or_else(|| "<lsof unavailable>".into());
            panic!(
                "pipe FD count mismatch during {context}: expected={expected}, current={current}\n{snapshot}"
            );
        }
    }
}

async fn spawn_many(garden: &Kindergarten, count: usize, command: fn() -> Command) -> io::Result<Vec<kindergarten::Ticket>> {
    let mut tickets = Vec::with_capacity(count);
    for _ in 0..count {
        tickets.push(garden.spawn(command()).await?);
    }
    Ok(tickets)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn exact_pipe_accounting_for_live_children_and_kill_cleanup() -> io::Result<()> {
    let _guard = TEST_LOCK.lock().expect("test lock poisoned");
    let Some(baseline) = current_pipe_fd_count()? else {
        eprintln!("skipping: `lsof` is not installed");
        return Ok(());
    };

    let garden = Arc::new(Kindergarten::default());
    let child_count = 24usize;
    let tickets = spawn_many(&garden, child_count, long_lived_command).await?;

    assert_pipe_fds_eventually(baseline + (child_count * 3), "steady-state live children").await?;

    let mut kill_set = JoinSet::new();
    for ticket in tickets {
        let garden = Arc::clone(&garden);
        kill_set.spawn(async move {
            let status = garden.kill(ticket).await.expect("child should exist")?;
            assert!(
                garden.get(ticket).is_none(),
                "child was not removed from the garden after kill"
            );
            Ok::<_, io::Error>(status)
        });
    }

    while let Some(result) = kill_set.join_next().await {
        result.expect("kill task panicked")?;
    }

    assert_pipe_fds_eventually(baseline, "post-kill cleanup").await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_mixed_completion_paths_never_leave_residual_pipes() -> io::Result<()> {
    let _guard = TEST_LOCK.lock().expect("test lock poisoned");
    let Some(baseline) = current_pipe_fd_count()? else {
        eprintln!("skipping: `lsof` is not installed");
        return Ok(());
    };

    let garden = Arc::new(Kindergarten::default());
    let rounds = 8usize;
    let tasks_per_round = 18usize;

    for round in 0..rounds {
        let mut set = JoinSet::new();

        for task_index in 0..tasks_per_round {
            let garden = Arc::clone(&garden);
            set.spawn(async move {
                match task_index % 3 {
                    0 => {
                        let ticket = garden.spawn(delayed_exit_command()).await?;
                        let leaked_clone = garden.get(ticket).expect("child should exist");
                        let status = garden.wait(ticket).await.expect("child should exist")?;
                        assert!(status.success(), "waited child exited unsuccessfully");
                        drop(leaked_clone);
                        assert!(garden.get(ticket).is_none(), "child should be removed after wait");
                    }
                    1 => {
                        let ticket = garden.spawn(delayed_exit_command()).await?;
                        let leaked_clone = garden.get(ticket).expect("child should exist");
                        loop {
                            if garden.has_terminated(ticket).await == Some(true) {
                                break;
                            }
                            sleep(Duration::from_millis(5)).await;
                        }
                        drop(leaked_clone);
                        assert!(
                            garden.get(ticket).is_none(),
                            "child should be removed after termination polling"
                        );
                    }
                    _ => {
                        let ticket = garden.spawn(long_lived_command()).await?;
                        let leaked_clone = garden.get(ticket).expect("child should exist");
                        sleep(Duration::from_millis(10)).await;
                        let _status = garden.kill(ticket).await.expect("child should exist")?;
                        drop(leaked_clone);
                        assert!(garden.get(ticket).is_none(), "child should be removed after kill");
                    }
                }

                Ok::<_, io::Error>(())
            });
        }

        while let Some(result) = set.join_next().await {
            result.expect("stress task panicked")?;
        }

        assert_pipe_fds_eventually(baseline, &format!("round {round} cleanup")).await?;
    }

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rapid_spawn_wait_cycles_return_to_exact_baseline_every_time() -> io::Result<()> {
    let _guard = TEST_LOCK.lock().expect("test lock poisoned");
    let Some(baseline) = current_pipe_fd_count()? else {
        eprintln!("skipping: `lsof` is not installed");
        return Ok(());
    };

    let garden = Kindergarten::default();

    for iteration in 0..128usize {
        let ticket = garden.spawn(quick_exit_command()).await?;
        let status = garden.wait(ticket).await.expect("child should exist")?;
        assert!(status.success(), "child exited unsuccessfully at iteration {iteration}");
        assert!(garden.get(ticket).is_none(), "child was not removed after wait");
        assert_pipe_fds_eventually(baseline, &format!("rapid iteration {iteration}")).await?;
    }

    Ok(())
}
