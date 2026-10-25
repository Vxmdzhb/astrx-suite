//! `astrx run`: role specs are parsed exactly, the selection is strict, a role
//! that exits is restarted on its own, and the real binary starts each role as
//! a child of itself with its output labelled.

#![cfg(feature = "net")]

use std::io::{BufRead, BufReader};
use std::process::{Command, ExitCode, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use astrx::dispatch::{split, Action};
use astrx::supervise::{parse_role, select, supervise, words, Pacing, Role};

fn s(v: &[&str]) -> Vec<String> {
    v.iter().map(|x| (*x).to_string()).collect()
}

#[test]
fn run_is_a_subcommand() {
    assert_eq!(
        split(&s(&["run", "a=gitweb"])),
        Action::Run(s(&["a=gitweb"]))
    );
}

#[test]
fn words_follow_the_documented_rules() {
    assert_eq!(words("a  b\tc").unwrap(), s(&["a", "b", "c"]));
    assert_eq!(
        words("--auth ''").unwrap(),
        s(&["--auth", ""]),
        "an empty quoted value is kept"
    );
    assert_eq!(words("'a b' \"c d\"").unwrap(), s(&["a b", "c d"]));
    assert_eq!(
        words(r#""say \"hi\" \\ \n""#).unwrap(),
        s(&[r#"say "hi" \ \n"#])
    );
    assert_eq!(words(r"a\ b").unwrap(), s(&["a b"]));
    assert_eq!(
        words("x='y z'w").unwrap(),
        s(&["x=y zw"]),
        "quotes join with adjacent text"
    );
    assert_eq!(
        words("$HOME `id`").unwrap(),
        s(&["$HOME", "`id`"]),
        "nothing is expanded"
    );
    assert!(words("'open").is_err());
    assert!(words("\"open").is_err());
    assert!(words("trailing\\").is_err());
}

#[test]
fn a_role_names_a_real_engine() {
    let r = parse_role("web=websearch serve --db /data/web.db --port 8803").unwrap();
    assert_eq!(
        r,
        Role {
            name: "web".into(),
            engine: "websearch".into(),
            args: s(&["serve", "--db", "/data/web.db", "--port", "8803"]),
        }
    );
    assert!(parse_role("websearch serve").is_err(), "name= is required");
    assert!(parse_role("Web=websearch").is_err(), "names are lowercase");
    assert!(parse_role("x=nosuchengine").is_err());
    assert!(
        parse_role("x=doctor").is_err(),
        "doctor is not a long-running role"
    );
    assert!(parse_role("x=run").is_err(), "no nested supervisors");
    assert!(parse_role("").is_err());
}

#[test]
fn the_selection_is_strict() {
    let specs = s(&["a=gitweb", "b=websearch serve", "c=torrentds index"]);
    let names = |only: Option<&str>| -> Vec<String> {
        select(&specs, only)
            .unwrap()
            .into_iter()
            .map(|r| r.name)
            .collect()
    };
    assert_eq!(names(None), s(&["a", "b", "c"]));
    assert_eq!(
        names(Some("")),
        s(&["a", "b", "c"]),
        "an empty selection means all"
    );
    assert_eq!(
        names(Some(" c , a ")),
        s(&["a", "c"]),
        "definition order, blanks ignored"
    );
    assert!(
        select(&specs, Some("a,zzz")).is_err(),
        "a selected name must exist"
    );
    assert!(
        select(&s(&["a=gitweb", "a=suitedash"]), None).is_err(),
        "names are unique"
    );
    assert!(select(&[], None).is_err(), "nothing to run is an error");
}

fn fast(max: u32) -> Pacing {
    Pacing {
        poll: Duration::from_millis(10),
        backoff_min: Duration::from_millis(10),
        backoff_max: Duration::from_millis(40),
        stable_after: Duration::from_secs(60),
        max_restarts: Some(max),
    }
}

fn role(name: &str) -> Role {
    Role {
        name: name.into(),
        engine: "gitweb".into(),
        args: vec![],
    }
}

#[test]
fn an_exiting_role_is_restarted_and_the_others_left_alone() {
    let dir = std::env::temp_dir().join(format!("astrx-run-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let launches = AtomicUsize::new(0);
    let code = supervise(vec![role("flaky"), role("steady")], fast(3), &|r: &Role| {
        launches.fetch_add(1, Ordering::SeqCst);
        let mut c = Command::new("sh");
        // flaky exits at once; steady runs long enough to see flaky cycle.
        let script = if r.name == "flaky" {
            format!("echo x >> {}/flaky", dir.display())
        } else {
            format!("echo x >> {}/steady; sleep 0.3", dir.display())
        };
        c.arg("-c").arg(script);
        c
    });
    let count = |f: &str| {
        std::fs::read_to_string(dir.join(f))
            .unwrap_or_default()
            .lines()
            .count()
    };
    assert_eq!(code, ExitCode::SUCCESS);
    assert_eq!(count("flaky"), 4, "the first run and three restarts");
    assert_eq!(count("steady"), 4);
    assert_eq!(launches.load(Ordering::SeqCst), 8);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_failing_role_is_reported_as_a_failure() {
    let code = supervise(vec![role("bad")], fast(1), &|_r: &Role| {
        let mut c = Command::new("sh");
        c.arg("-c").arg("exit 3");
        c
    });
    assert_eq!(code, ExitCode::FAILURE);
}

#[test]
fn a_role_that_cannot_start_is_retried_then_given_up() {
    let code = supervise(vec![role("ghost")], fast(2), &|_r: &Role| {
        Command::new("/nonexistent/astrx-test-binary")
    });
    assert_eq!(code, ExitCode::FAILURE);
}

#[test]
fn the_backoff_doubles() {
    // Three quick exits with a 40 ms floor doubling to 160 ms: at least
    // 40 + 80 + 160 ms pass before the fourth run.
    let pacing = Pacing {
        poll: Duration::from_millis(5),
        backoff_min: Duration::from_millis(40),
        backoff_max: Duration::from_millis(160),
        stable_after: Duration::from_secs(60),
        max_restarts: Some(3),
    };
    let t = Instant::now();
    let code = supervise(vec![role("quick")], pacing, &|_r: &Role| {
        let mut c = Command::new("true");
        c.stdout(Stdio::null());
        c
    });
    assert_eq!(code, ExitCode::SUCCESS);
    assert!(
        t.elapsed() >= Duration::from_millis(280),
        "{:?}",
        t.elapsed()
    );
}

#[test]
fn the_real_binary_runs_each_role_as_a_labelled_child_of_itself() {
    // Each role prints its engine's --help and exits; the supervisor would
    // restart them forever, so the test reads what it needs and stops it.
    let mut child = Command::new(env!("CARGO_BIN_EXE_astrx"))
        .args([
            "run",
            "--roles",
            "dash",
            "git=gitweb --help",
            "dash=suitedash --help",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("astrx runs");
    let out = child.stdout.take().unwrap();
    let err = child.stderr.take().unwrap();
    let err_lines = std::thread::spawn(move || {
        BufReader::new(err)
            .lines()
            .map_while(Result::ok)
            .take(2)
            .collect::<Vec<_>>()
    });
    let mut seen = Vec::new();
    for line in BufReader::new(out).lines().map_while(Result::ok) {
        seen.push(line);
        if seen.len() >= 3 {
            break;
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    assert!(!seen.is_empty(), "no output from the role");
    assert!(seen.iter().all(|l| l.starts_with("[dash] ")), "{seen:?}");
    assert!(
        seen.iter().any(|l| l.to_lowercase().contains("suitedash")),
        "{seen:?}"
    );
    let notes = err_lines.join().unwrap();
    assert!(
        notes
            .iter()
            .any(|l| l.contains("supervising dash (suitedash)")),
        "{notes:?}"
    );
    assert!(
        notes.iter().all(|l| !l.contains("git (")),
        "the unselected role stays off: {notes:?}"
    );
}
