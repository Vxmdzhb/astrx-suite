//! Running git (and cargo) as child processes.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

/// Run `program args` in `dir` with extra environment and optional stdin;
/// its stdout, or an error naming the command and quoting its stderr.
pub fn run_in(
    program: &str,
    dir: &Path,
    args: &[&str],
    env: &[(&str, &str)],
    stdin: Option<&[u8]>,
) -> Result<Vec<u8>, String> {
    let mut cmd = Command::new(program);
    cmd.current_dir(dir)
        .args(args)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in env {
        cmd.env(k, v);
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("{program} {}: could not start: {e}", args.join(" ")))?;
    // Feed stdin from a thread, so a child that writes a lot before it has
    // read everything cannot deadlock against us.
    let writer = stdin.map(|data| {
        let mut pipe = child.stdin.take().expect("stdin is piped");
        let data = data.to_vec();
        std::thread::spawn(move || {
            let _ = pipe.write_all(&data);
        })
    });
    let out = child
        .wait_with_output()
        .map_err(|e| format!("{program} {}: {e}", args.join(" ")))?;
    if let Some(w) = writer {
        let _ = w.join();
    }
    if !out.status.success() {
        return Err(format!(
            "{program} {} ({}):\n{}",
            args.join(" "),
            out.status,
            String::from_utf8_lossy(&out.stderr).trim_end()
        ));
    }
    Ok(out.stdout)
}

/// git in `dir`; raw stdout.
pub fn git(dir: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    run_in("git", dir, args, &[], None)
}

/// git in `dir`; stdout as trimmed text.
pub fn git_str(dir: &Path, args: &[&str]) -> Result<String, String> {
    let out = git(dir, args)?;
    Ok(String::from_utf8_lossy(&out).trim().to_string())
}

/// git in `dir` with environment and stdin; raw stdout.
pub fn git_with(
    dir: &Path,
    args: &[&str],
    env: &[(&str, &str)],
    stdin: Option<&[u8]>,
) -> Result<Vec<u8>, String> {
    run_in("git", dir, args, env, stdin)
}
