//! astrx-split — one repository per crate, with history.
//!
//! ```text
//! cargo run --manifest-path tools/split/Cargo.toml -- \
//!     --run --sign --repo-url https://github.com/Vxmdzhb/{name} \
//!     [--push git@github.com:Vxmdzhb/{name}.git]
//! ```
//!
//! Splits the workspace at HEAD into:
//!
//! - `split/crawlcore`, then `split/gitweb`, `split/onioncrawler`,
//!   `split/suitedash`, `split/torrentds` (with `fuzz/`), `split/websearch`:
//!   each crate with the full history of its files, followed back through
//!   their renames, plus one commit that makes it a repository of its own —
//!   its manifest points at its repository and gets crawlcore from crawlcore's,
//!   a lockfile pinning crawlcore's exact final commit, the suite's toolchain,
//!   format and dependency policy, a README and a CI workflow;
//! - `split/astrx-suite`: this repository's HEAD plus one commit that removes
//!   the crates that left, gets them from their repositories (lockfile pinned
//!   to each one's final commit) and updates CI, cargo-deny and the README. It
//!   is a fast-forward of the current branch: push it to the suite's `main`.
//!
//! The lockfiles are exact although none of the new repositories exists yet:
//! each finished part is copied to a local mirror, and Cargo is pointed at
//! those mirrors (git `url.<mirror>.insteadOf <repo-url>`) while it writes the
//! lockfiles. They record the real URLs.
//!
//! `--sign` signs every commit of every part with this repository's signing
//! key, keeping each history's exact shape (see `history::sign`). Without it
//! the branches are unsigned: extraction rewrites commits, so their old
//! signatures cannot carry over.
//!
//! `--push` pushes each part to its repository's `main` as soon as it is done
//! (they must exist, empty), and the suite branch to the suite repository's
//! `main` (a fast-forward). Without it, push the `split/*` branches yourself.

mod adapt;
mod git;
mod history;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use adapt::{Change, Ctx, ENGINES, LIBRARY, SUITE};
use git::{git, git_str, git_with, run_in};

struct Opts {
    run: bool,
    sign: bool,
    repo_url: Option<String>,
    push: Option<String>,
}

const USAGE: &str =
    "usage: astrx-split [--run] [--sign] --repo-url URL-with-{name} [--push URL-with-{name}]\n\
\n\
  --repo-url  where the crates will live, e.g. https://github.com/Vxmdzhb/{name}\n\
  --run       create the split/* branches (without it: list what would be done)\n\
  --sign      sign every commit with this repository's signing key\n\
  --push      push each branch to its repository's main (implies --run)\n";

fn parse(args: &[String]) -> Result<Opts, String> {
    let mut o = Opts {
        run: false,
        sign: false,
        repo_url: None,
        push: None,
    };
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        let mut value = |name: &str| -> Result<String, String> {
            if let Some(v) = a.strip_prefix(&format!("{name}=")) {
                return Ok(v.to_string());
            }
            i += 1;
            args.get(i).cloned().ok_or(format!("{name} needs a value"))
        };
        match a {
            "-h" | "--help" => return Err(String::new()),
            "--run" => o.run = true,
            "--sign" => {
                o.sign = true;
                o.run = true;
            }
            _ if a == "--repo-url" || a.starts_with("--repo-url=") => {
                o.repo_url = Some(value("--repo-url")?)
            }
            _ if a == "--push" || a.starts_with("--push=") => {
                o.push = Some(value("--push")?);
                o.run = true;
            }
            other => return Err(format!("unknown argument {other}")),
        }
        i += 1;
    }
    match &o.repo_url {
        None => return Err("--repo-url is required".into()),
        Some(u) if !u.contains("{name}") => return Err("--repo-url needs {name}".into()),
        Some(u) if !u.starts_with("https://") => {
            return Err(
                "--repo-url must be an https:// URL (it ends up in manifests and lockfiles)".into(),
            )
        }
        _ => {}
    }
    if o.push.as_deref().is_some_and(|p| !p.contains("{name}")) {
        return Err("--push needs {name}".into());
    }
    Ok(o)
}

/// The paths a crate keeps, and their names in its repository.
fn sources(name: &str) -> Vec<(String, String)> {
    let mut v = vec![
        (format!("crates/{name}/"), String::new()),
        ("crates/GOLDENS.md".into(), "GOLDENS.md".into()),
    ];
    if name == "torrentds" {
        v.push(("fuzz/".into(), "fuzz/".into()));
    }
    v
}

/// What builds a part's changes, given its checkout.
type MakeChanges<'a> = &'a dyn Fn(&Path) -> Result<Vec<Change>, String>;
/// What writes a part's lockfile, given its checkout.
type MakeLock<'a> = &'a dyn Fn(&Path) -> Result<Vec<u8>, String>;

/// Local copies of the finished parts, and the git config that makes Cargo
/// fetch their repository URLs from them.
struct Mirrors {
    dir: PathBuf,
    gitconfig: PathBuf,
    entries: Vec<(String, PathBuf)>,
}

impl Mirrors {
    fn publish(&mut self, root: &Path, branch: &str, url: &str, name: &str) -> Result<(), String> {
        let path = self.dir.join(format!("{name}.git"));
        if !path.exists() {
            git(
                &self.dir,
                &[
                    "init",
                    "-q",
                    "--bare",
                    "-b",
                    "main",
                    path.to_str().unwrap_or_default(),
                ],
            )?;
        }
        git(
            root,
            &[
                "push",
                "-q",
                "--force",
                path.to_str().unwrap_or_default(),
                &format!("{branch}:refs/heads/main"),
            ],
        )?;
        self.entries.retain(|(u, _)| u != url);
        self.entries.push((url.to_string(), path));
        let mut cfg = String::new();
        for (u, p) in &self.entries {
            cfg.push_str(&format!(
                "[url \"file://{}\"]\n\tinsteadOf = {u}\n",
                p.display()
            ));
        }
        std::fs::write(&self.gitconfig, cfg)
            .map_err(|e| format!("{}: {e}", self.gitconfig.display()))
    }
}

/// The lockfile for the manifest(s) at `dir`: the suite's lockfile, adjusted
/// by Cargo to what `dir` actually needs (third-party versions kept), with
/// the suite's own git crates fetched from the mirrors.
fn lockfile(ctx: &Ctx<'_>, dir: &Path, mirrors: &Mirrors) -> Result<Vec<u8>, String> {
    std::fs::write(dir.join("Cargo.lock"), ctx.suite_file("Cargo.lock")?)
        .map_err(|e| e.to_string())?;
    let cfg = mirrors.gitconfig.to_str().unwrap_or_default().to_string();
    run_in(
        "cargo",
        dir,
        &["fetch", "--quiet"],
        &[
            ("GIT_CONFIG_GLOBAL", &cfg),
            ("GIT_CONFIG_NOSYSTEM", "1"),
            ("CARGO_NET_GIT_FETCH_WITH_CLI", "true"),
        ],
        None,
    )?;
    std::fs::read(dir.join("Cargo.lock")).map_err(|e| e.to_string())
}

/// Commit on top of `base`, in a scratch clone: the changes `make` returns
/// (given the checkout), then — if `lock` — a lockfile for the result. The
/// new commit is fetched back into `root`; its id is returned.
fn commit_on(
    root: &Path,
    work: &Path,
    base: &str,
    message: &str,
    author: &[(&str, &str)],
    make: MakeChanges<'_>,
    lock: Option<MakeLock<'_>>,
) -> Result<String, String> {
    let dir = work.join(format!("checkout-{}", &base[..12.min(base.len())]));
    let _ = std::fs::remove_dir_all(&dir);
    git(
        work,
        &[
            "clone",
            "-q",
            "--no-local",
            "--no-checkout",
            root.to_str().unwrap_or_default(),
            dir.to_str().unwrap_or_default(),
        ],
    )?;
    git(&dir, &["checkout", "-q", "--detach", base])?;
    for (path, content) in make(&dir)? {
        let full = dir.join(&path);
        match content {
            Some(bytes) => {
                if let Some(parent) = full.parent() {
                    std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
                }
                std::fs::write(&full, bytes).map_err(|e| format!("{path}: {e}"))?;
            }
            None => {
                if full.exists() {
                    git(&dir, &["rm", "-r", "-q", "--", &path])?;
                }
            }
        }
    }
    if let Some(lock) = lock {
        let bytes = lock(&dir)?;
        std::fs::write(dir.join("Cargo.lock"), bytes).map_err(|e| e.to_string())?;
    }
    git(&dir, &["add", "-A"])?;
    let _ = std::fs::remove_dir_all(dir.join("target"));
    git_with(
        &dir,
        &["-c", "commit.gpgsign=false", "commit", "-q", "-m", message],
        author,
        None,
    )?;
    let sha = git_str(&dir, &["rev-parse", "HEAD"])?;
    git(
        root,
        &["fetch", "-q", dir.to_str().unwrap_or_default(), &sha],
    )?;
    let _ = std::fs::remove_dir_all(&dir);
    Ok(sha)
}

fn real_main(o: &Opts) -> Result<(), String> {
    let root = PathBuf::from(git_str(Path::new("."), &["rev-parse", "--show-toplevel"])?);
    let repo_url = o.repo_url.as_deref().unwrap_or_default();
    let ctx = Ctx {
        suite: &root,
        repo_url,
    };
    let parts: Vec<&str> = std::iter::once(LIBRARY).chain(ENGINES).collect();
    for p in &parts {
        git(
            &root,
            &["cat-file", "-e", &format!("HEAD:crates/{p}/Cargo.toml")],
        )
        .map_err(|_| {
            format!(
                "HEAD has no crates/{p}: run this from the astrx-suite workspace before the split"
            )
        })?;
    }

    if !o.run {
        for p in &parts {
            println!("  split/{p:<14} crates/{p}/ → {}", ctx.url(p));
        }
        println!(
            "  split/{SUITE:<14} HEAD + the crates from their repositories → {}",
            ctx.url(SUITE)
        );
        println!("\nRun with --run (and --sign, --push) to do it.");
        return Ok(());
    }

    let name = git_str(&root, &["config", "user.name"])
        .map_err(|_| "git user.name is not set".to_string())?;
    let email = git_str(&root, &["config", "user.email"])
        .map_err(|_| "git user.email is not set".to_string())?;
    let author = [
        ("GIT_AUTHOR_NAME", name.as_str()),
        ("GIT_AUTHOR_EMAIL", email.as_str()),
        ("GIT_COMMITTER_NAME", name.as_str()),
        ("GIT_COMMITTER_EMAIL", email.as_str()),
    ];
    let head = git_str(&root, &["rev-parse", "HEAD"])?;
    let work = std::env::temp_dir().join(format!("astrx-split-{}", std::process::id()));
    std::fs::create_dir_all(work.join("mirrors")).map_err(|e| e.to_string())?;
    let mut mirrors = Mirrors {
        dir: work.join("mirrors"),
        gitconfig: work.join("gitconfig"),
        entries: Vec::new(),
    };
    std::fs::write(&mirrors.gitconfig, "").map_err(|e| e.to_string())?;

    eprintln!("finding renames…");
    let renames = history::renames(&root, "HEAD", 75)?;

    for p in &parts {
        let branch = format!("split/{p}");
        let src = sources(p);
        let src_refs: Vec<(&str, &str)> =
            src.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
        let mapper = history::Mapper::new(&root, "HEAD", &src_refs, &renames)?;
        let extracted = history::rewrite(&root, "HEAD", &mapper, &format!("refs/heads/{branch}"))?;
        let message = format!(
            "Make {p} a repository of its own\n\n\
             It was a crate of the astrx-suite workspace. Its manifest now points at\n\
             this repository{dep}, and it carries the suite's toolchain, format and\n\
             dependency policy, a lockfile, a README and a CI workflow.",
            dep = if *p == LIBRARY {
                String::new()
            } else {
                format!(" and gets {LIBRARY} from {}", ctx.url(LIBRARY))
            },
        );
        let make = |dir: &Path| adapt::crate_part(&ctx, p, dir);
        let lock = |dir: &Path| lockfile(&ctx, dir, &mirrors);
        let sha = commit_on(
            &root,
            &work,
            &extracted,
            &message,
            &author,
            &make,
            Some(&lock),
        )?;
        git(&root, &["branch", "-f", &branch, &sha])?;
        let sha = if o.sign {
            history::sign(&root, &branch, None)?
        } else {
            sha
        };
        mirrors.publish(&root, &branch, &ctx.url(p), p)?;
        let commits = git_str(&root, &["rev-list", "--count", &branch])?;
        let mut line = format!("{branch:<20} {} ({commits} commits)", &sha[..10]);
        if let Some(push) = &o.push {
            let url = push.replace("{name}", p);
            git(
                &root,
                &["push", "-q", &url, &format!("{branch}:refs/heads/main")],
            )?;
            line.push_str(&format!("  → {url}"));
        }
        println!("{line}");
    }

    // The suite itself: HEAD + the engines from their repositories.
    let branch = format!("split/{SUITE}");
    let message = "Get the engines from their own repositories\n\n\
        crawlcore and the five engines are each a repository of their own now,\n\
        with their history. This repository keeps the crate that assembles them\n\
        into the single astrx binary, and the deployment: crates/astrx gets them\n\
        as git dependencies, pinned by Cargo.lock to each one's exact commit.\n\
        CI and cargo-deny count them as first party; the gitweb/git matrix and\n\
        the fuzz harnesses moved with their crates.";
    let make = |dir: &Path| adapt::suite_part(&ctx, dir);
    let lock = |dir: &Path| lockfile(&ctx, dir, &mirrors);
    let sha = commit_on(&root, &work, &head, message, &author, &make, Some(&lock))?;
    git(&root, &["branch", "-f", &branch, &sha])?;
    let sha = if o.sign {
        history::sign(&root, &branch, Some(&head))?
    } else {
        sha
    };
    let mut line = format!("{branch:<20} {} (HEAD + 1 commit)", &sha[..10]);
    if let Some(push) = &o.push {
        let url = push.replace("{name}", SUITE);
        git(
            &root,
            &["push", "-q", &url, &format!("{branch}:refs/heads/main")],
        )?;
        line.push_str(&format!("  → {url}"));
    }
    println!("{line}");
    let _ = std::fs::remove_dir_all(&work);
    Ok(())
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let opts = match parse(&args) {
        Ok(o) => o,
        Err(e) if e.is_empty() => {
            print!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            eprintln!("{USAGE}\nerror: {e}");
            return ExitCode::from(2);
        }
    };
    match real_main(&opts) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}
