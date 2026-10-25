//! `astrx run` — several engine roles in one process tree.
//!
//! One container per role means eleven containers for the full suite. Most
//! of them share a network, a volume and an image, so nothing but habit keeps
//! them apart. `astrx run` starts several roles from one binary and keeps
//! them running:
//!
//! ```text
//! astrx run "gitweb=gitweb --root /repos --port 8801" \
//!           "web=websearch serve --db /data/web.db --port 8803"
//! ```
//!
//! Each argument is one role: a name, `=`, and the engine command line exactly
//! as `astrx <engine> …` takes it. Words are split on whitespace; single
//! quotes keep text literally, double quotes allow `\"` and `\\`, and a
//! backslash outside quotes escapes the next character. No shell is involved,
//! so nothing is expanded.
//!
//! - **Which roles run.** All of them, unless `--roles a,b` or the
//!   `ASTRX_ROLES` environment variable (same format; `--roles` wins) names
//!   some. That is how a compose file lists every role a container *can* run
//!   and leaves the optional ones off by default.
//! - **Logs.** Every line a role writes is prefixed with `[name] `, on the
//!   same stream (stdout or stderr) it was written to.
//! - **Restarts.** A role that exits is started again after a delay that
//!   doubles from one second up to a minute, and drops back to one second once
//!   the role has stayed up for a minute — so a crash loop does not spin, and
//!   one bad night does not leave a role waiting a minute forever after. The
//!   other roles are not touched.
//! - **Stopping.** Run it under an init process (`init: true` in compose).
//!   The engines have no signal handlers of their own; with an init, the stop
//!   signal ends them as it always did, instead of being ignored by a PID 1
//!   until Docker's kill timeout.
//!
//! Each role is a child process running this same binary, so a role that
//! panics or exits takes nothing else down with it.

use std::collections::HashSet;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{Child, Command, ExitCode, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::dispatch::ENGINES;

/// One role: a name, the engine it runs and the engine's arguments.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Role {
    /// Unique label: log prefix and `--roles` / `ASTRX_ROLES` key.
    pub name: String,
    /// The engine (`gitweb`, `websearch`, …).
    pub engine: String,
    /// Everything after the engine name.
    pub args: Vec<String>,
}

/// Split a command line into words (see the module docs for the rules).
///
/// # Errors
/// An unterminated quote, or a trailing backslash.
pub fn words(line: &str) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_word = false;
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match c {
            c if c.is_whitespace() => {
                if in_word {
                    out.push(std::mem::take(&mut cur));
                    in_word = false;
                }
            }
            '\'' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(ch) => cur.push(ch),
                        None => return Err(format!("unterminated ' in: {line}")),
                    }
                }
            }
            '"' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            Some(ch @ ('"' | '\\')) => cur.push(ch),
                            Some(ch) => {
                                cur.push('\\');
                                cur.push(ch);
                            }
                            None => return Err(format!("unterminated \" in: {line}")),
                        },
                        Some(ch) => cur.push(ch),
                        None => return Err(format!("unterminated \" in: {line}")),
                    }
                }
            }
            '\\' => {
                in_word = true;
                match chars.next() {
                    Some(ch) => cur.push(ch),
                    None => return Err(format!("trailing \\ in: {line}")),
                }
            }
            c => {
                in_word = true;
                cur.push(c);
            }
        }
    }
    if in_word {
        out.push(cur);
    }
    Ok(out)
}

/// Parse one role spec: `name=engine args…`.
///
/// # Errors
/// A malformed spec, a bad name, or an engine that does not exist.
pub fn parse_role(spec: &str) -> Result<Role, String> {
    let mut w = words(spec)?;
    if w.is_empty() {
        return Err("empty role".to_string());
    }
    let head = w.remove(0);
    let Some((name, engine)) = head.split_once('=') else {
        return Err(format!("role {spec:?}: expected name=engine at the start"));
    };
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
    {
        return Err(format!(
            "role {spec:?}: the name must be lowercase letters, digits, - or _"
        ));
    }
    if !ENGINES.iter().any(|e| e.name == engine) {
        return Err(format!(
            "role {name}: unknown engine {engine:?} (expected one of: {})",
            ENGINES
                .iter()
                .map(|e| e.name)
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    Ok(Role {
        name: name.to_string(),
        engine: engine.to_string(),
        args: w,
    })
}

/// The roles to run: `specs` parsed, then narrowed to `only` when it names
/// any (comma-separated; blanks ignored).
///
/// # Errors
/// A bad spec, a duplicate name, a name in `only` that no spec has, or
/// nothing left to run.
pub fn select(specs: &[String], only: Option<&str>) -> Result<Vec<Role>, String> {
    let mut roles = Vec::new();
    let mut seen = HashSet::new();
    for spec in specs {
        let role = parse_role(spec)?;
        if !seen.insert(role.name.clone()) {
            return Err(format!("role {}: defined twice", role.name));
        }
        roles.push(role);
    }
    if let Some(only) = only {
        let wanted: Vec<&str> = only
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect();
        if !wanted.is_empty() {
            for w in &wanted {
                if !seen.contains(*w) {
                    return Err(format!(
                        "role {w:?} is selected but not defined (defined: {})",
                        roles
                            .iter()
                            .map(|r| r.name.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ));
                }
            }
            roles.retain(|r| wanted.contains(&r.name.as_str()));
        }
    }
    if roles.is_empty() {
        return Err("no role to run".to_string());
    }
    Ok(roles)
}

/// How the supervisor paces itself. [`Pacing::default`] is what `astrx run`
/// uses; tests shorten it.
#[derive(Clone, Copy, Debug)]
pub struct Pacing {
    /// How often children are checked.
    pub poll: Duration,
    /// First restart delay.
    pub backoff_min: Duration,
    /// Longest restart delay.
    pub backoff_max: Duration,
    /// Up this long, a role's next restart delay drops back to the minimum.
    pub stable_after: Duration,
    /// Stop restarting a role after this many restarts (`None`: never stop).
    pub max_restarts: Option<u32>,
}

impl Default for Pacing {
    fn default() -> Self {
        Pacing {
            poll: Duration::from_millis(200),
            backoff_min: Duration::from_secs(1),
            backoff_max: Duration::from_secs(60),
            stable_after: Duration::from_secs(60),
            max_restarts: None,
        }
    }
}

struct Slot {
    role: Role,
    child: Option<Child>,
    started: Instant,
    next_start: Instant,
    backoff: Duration,
    restarts: u32,
    done: bool,
    last_ok: bool,
}

/// Copy `from` to stdout or stderr line by line, each line prefixed.
fn relay<R: Read + Send + 'static>(from: R, prefix: String, to_stderr: bool) {
    thread::spawn(move || {
        let mut reader = BufReader::new(from);
        let mut line = Vec::new();
        loop {
            line.clear();
            match reader.read_until(b'\n', &mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    if line.last() != Some(&b'\n') {
                        line.push(b'\n');
                    }
                    let mut buf = Vec::with_capacity(prefix.len() + line.len());
                    buf.extend_from_slice(prefix.as_bytes());
                    buf.extend_from_slice(&line);
                    // One write per line, under the stream's lock, so lines
                    // from different roles never interleave mid-line.
                    if to_stderr {
                        let _ = std::io::stderr().lock().write_all(&buf);
                    } else {
                        let _ = std::io::stdout().lock().write_all(&buf);
                    }
                }
            }
        }
    });
}

fn note(msg: &str) {
    let _ = writeln!(std::io::stderr().lock(), "astrx run: {msg}");
}

/// Run `roles` until every one has stopped for good — with the default
/// pacing, never. `launch` builds the command for a role.
///
/// Returns success when every role's last run exited successfully.
pub fn supervise(roles: Vec<Role>, pacing: Pacing, launch: &dyn Fn(&Role) -> Command) -> ExitCode {
    let now = Instant::now();
    let mut slots: Vec<Slot> = roles
        .into_iter()
        .map(|role| Slot {
            role,
            child: None,
            started: now,
            next_start: now,
            backoff: pacing.backoff_min,
            restarts: 0,
            done: false,
            last_ok: true,
        })
        .collect();

    loop {
        let now = Instant::now();
        for s in &mut slots {
            if let Some(child) = s.child.as_mut() {
                match child.try_wait() {
                    Ok(Some(status)) => {
                        s.child = None;
                        s.last_ok = status.success();
                        if now.duration_since(s.started) >= pacing.stable_after {
                            s.backoff = pacing.backoff_min;
                        }
                        if pacing.max_restarts.is_some_and(|max| s.restarts >= max) {
                            s.done = true;
                            note(&format!(
                                "[{}] exited ({status}); not restarting",
                                s.role.name
                            ));
                        } else {
                            note(&format!(
                                "[{}] exited ({status}); restarting in {}s",
                                s.role.name,
                                s.backoff.as_secs_f32()
                            ));
                            s.next_start = now + s.backoff;
                            s.backoff = (s.backoff * 2).min(pacing.backoff_max);
                            s.restarts += 1;
                        }
                    }
                    Ok(None) => {}
                    Err(e) => note(&format!("[{}] could not be checked: {e}", s.role.name)),
                }
            }
            if s.child.is_none() && !s.done && now >= s.next_start {
                let mut cmd = launch(&s.role);
                cmd.stdin(Stdio::null())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped());
                match cmd.spawn() {
                    Ok(mut child) => {
                        let prefix = format!("[{}] ", s.role.name);
                        if let Some(out) = child.stdout.take() {
                            relay(out, prefix.clone(), false);
                        }
                        if let Some(err) = child.stderr.take() {
                            relay(err, prefix, true);
                        }
                        note(&format!("[{}] started (pid {})", s.role.name, child.id()));
                        s.started = now;
                        s.child = Some(child);
                    }
                    Err(e) => {
                        s.last_ok = false;
                        if pacing.max_restarts.is_some_and(|max| s.restarts >= max) {
                            s.done = true;
                            note(&format!(
                                "[{}] could not start: {e}; giving up",
                                s.role.name
                            ));
                        } else {
                            note(&format!(
                                "[{}] could not start: {e}; retrying in {}s",
                                s.role.name,
                                s.backoff.as_secs_f32()
                            ));
                            s.next_start = now + s.backoff;
                            s.backoff = (s.backoff * 2).min(pacing.backoff_max);
                            s.restarts += 1;
                        }
                    }
                }
            }
        }
        if slots.iter().all(|s| s.done) {
            return if slots.iter().all(|s| s.last_ok) {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            };
        }
        thread::sleep(pacing.poll);
    }
}

/// Usage text for `astrx run`.
#[must_use]
pub fn usage() -> String {
    "usage: astrx run [--roles name,…] 'name=engine args…' …\n\
     \n\
     Starts every role (or only those --roles / ASTRX_ROLES name) as a child\n\
     process of this binary, prefixes their output with [name], and restarts\n\
     a role that exits (1s, doubling up to 60s). Run it under an init process.\n"
        .to_string()
}

/// `astrx run …`: parse the arguments and supervise forever.
pub fn run(argv: &[String]) -> ExitCode {
    let mut specs = Vec::new();
    let mut only: Option<String> = std::env::var("ASTRX_ROLES").ok();
    let mut i = 0;
    while i < argv.len() {
        match argv[i].as_str() {
            "-h" | "--help" => {
                print!("{}", usage());
                return ExitCode::SUCCESS;
            }
            "--roles" => {
                let Some(v) = argv.get(i + 1) else {
                    eprintln!("{}\nerror: --roles needs a value", usage());
                    return ExitCode::from(2);
                };
                only = Some(v.clone());
                i += 1;
            }
            a if a.starts_with("--roles=") => only = Some(a["--roles=".len()..].to_string()),
            a => specs.push(a.to_string()),
        }
        i += 1;
    }
    let roles = match select(&specs, only.as_deref()) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("{}\nerror: {e}", usage());
            return ExitCode::from(2);
        }
    };
    let exe: PathBuf = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: cannot find this binary to start the roles: {e}");
            return ExitCode::from(70);
        }
    };
    note(&format!(
        "supervising {}",
        roles
            .iter()
            .map(|r| format!("{} ({})", r.name, r.engine))
            .collect::<Vec<_>>()
            .join(", ")
    ));
    supervise(roles, Pacing::default(), &|role: &Role| {
        let mut cmd = Command::new(&exe);
        cmd.arg(&role.engine).args(&role.args);
        cmd
    })
}
