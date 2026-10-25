//! What changes in each part so it works as a repository of its own.
//!
//! Every edit of a file the suite also has is an exact replacement that must
//! apply exactly once: when that file has changed here, the split stops with
//! the file's name instead of producing a repository that silently misses the
//! edit. Fix the replacement here, then split again.

use std::path::Path;

use crate::git::git;

/// The shared library, then the engines (the order the split runs in: the
/// engines' lockfiles pin crawlcore's final commit).
pub const LIBRARY: &str = "crawlcore";
/// The engine crates.
pub const ENGINES: [&str; 5] = [
    "gitweb",
    "onioncrawler",
    "suitedash",
    "torrentds",
    "websearch",
];
/// The suite repository itself, which assembles the engines.
pub const SUITE: &str = "astrx-suite";

/// A file to write (`Some`) or remove (`None`), relative to the part's root.
pub type Change = (String, Option<Vec<u8>>);

/// Settings shared by every adaptation.
pub struct Ctx<'a> {
    /// The suite repository (its HEAD is what is split).
    pub suite: &'a Path,
    /// Where the parts will live, with `{name}`.
    pub repo_url: &'a str,
}

impl Ctx<'_> {
    /// A part's repository URL.
    pub fn url(&self, name: &str) -> String {
        self.repo_url.replace("{name}", name)
    }

    /// The URL up to the part's name: what every part's git source starts with.
    pub fn url_prefix(&self) -> String {
        self.repo_url
            .split("{name}")
            .next()
            .unwrap_or("")
            .to_string()
    }

    /// A file of the suite at HEAD.
    pub fn suite_file(&self, path: &str) -> Result<String, String> {
        let out = git(self.suite, &["show", &format!("HEAD:{path}")])?;
        String::from_utf8(out).map_err(|_| format!("{path} is not UTF-8"))
    }
}

/// Apply `[old, new]` replacements; each old text must occur exactly once.
pub fn edit(file: &str, mut content: String, edits: &[(&str, &str)]) -> Result<String, String> {
    for (old, new) in edits {
        let n = content.matches(old).count();
        if n != 1 {
            return Err(format!(
                "{file}: expected this text exactly once, found it {n} time(s) — update tools/split/src/adapt.rs:\n{old}"
            ));
        }
        content = content.replacen(old, new, 1);
    }
    Ok(content)
}

fn read(dir: &Path, path: &str) -> Result<String, String> {
    std::fs::read_to_string(dir.join(path)).map_err(|e| format!("{path}: {e}"))
}

/// The features a manifest declares, in order, without `default`.
fn features(manifest: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut inside = false;
    for line in manifest.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            inside = t == "[features]";
            continue;
        }
        if inside {
            if let Some((k, _)) = t.split_once('=') {
                let k = k.trim();
                if !k.is_empty() && !k.starts_with('#') && k != "default" {
                    out.push(k.to_string());
                }
            }
        }
    }
    out
}

fn manifest_value(manifest: &str, key: &str) -> Option<String> {
    manifest.lines().find_map(|l| {
        let (k, v) = l.split_once('=')?;
        (k.trim() == key).then(|| v.trim().trim_matches('"').to_string())
    })
}

/// The suite's deny.toml, allowing exactly `git_urls` as git sources.
fn deny_toml(ctx: &Ctx<'_>, git_urls: &[String]) -> Result<String, String> {
    let deny = ctx.suite_file("deny.toml")?;
    if git_urls.is_empty() {
        return Ok(deny);
    }
    let list = git_urls
        .iter()
        .map(|u| format!("  \"{u}\","))
        .collect::<Vec<_>>()
        .join("\n");
    edit(
        "deny.toml",
        deny,
        &[(
            "allow-registry = [\"https://github.com/rust-lang/crates.io-index\"]",
            &format!(
                "allow-registry = [\"https://github.com/rust-lang/crates.io-index\"]\n\
                 # The suite's own crates, each in its repository.\nallow-git = [\n{list}\n]"
            ),
        )],
    )
}

const GITIGNORE: &str = "/target/\n**/*.rs.bk\n\n# Local runtime state: an index/tracker DB is machine-local, never committed\n*.db\n*.db-journal\n*.db-wal\n*.db-shm\npeers.json\n.env\n";

/// The CI workflow of one crate's repository: the suite's gates, for this
/// crate alone.
fn crate_ci(ctx: &Ctx<'_>, name: &str, manifest: &str) -> Result<String, String> {
    let suite_ci = ctx.suite_file(".github/workflows/ci.yml")?;
    let lint = suite_ci
        .lines()
        .find_map(|l| l.trim().strip_prefix("LINT_TOOLCHAIN:"))
        .map(|v| v.trim().trim_matches('\'').trim_matches('"').to_string())
        .ok_or(
            "the suite's ci.yml no longer sets LINT_TOOLCHAIN — update tools/split/src/adapt.rs",
        )?;
    let msrv =
        manifest_value(manifest, "rust-version").ok_or(format!("{name}: no rust-version"))?;
    let msrv_full = if msrv.matches('.').count() == 1 {
        format!("{msrv}.0")
    } else {
        msrv.clone()
    };
    let feats = features(manifest);
    let mut matrix = vec!["'--no-default-features'".to_string(), "''".to_string()];
    for f in &feats {
        matrix.push(format!("'--features {f}'"));
    }
    matrix.push("'--all-features'".to_string());

    let mut yml = CRATE_CI
        .replace("@NAME@", name)
        .replace("@LINT@", &lint)
        .replace("@MSRV@", &msrv)
        .replace("@MSRV_FULL@", &msrv_full)
        .replace("@MATRIX@", &matrix.join(", "))
        .replace("@PREFIX@", &format!("git+{}", ctx.url_prefix()));
    if name == "gitweb" {
        yml.push_str(GITWEB_CI);
    }
    Ok(yml)
}

const CRATE_CI: &str = r#"name: CI

on:
  push:
  pull_request:

permissions:
  contents: read

env:
  CARGO_TERM_COLOR: always
  # The lint bar is a pinned toolchain, as in the suite: clippy changes its
  # lints between releases, and a bar that moves by itself is not a bar.
  LINT_TOOLCHAIN: '@LINT@'

jobs:
  fmt:
    name: rustfmt
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@master
        with:
          toolchain: ${{ env.LINT_TOOLCHAIN }}
          components: rustfmt
      - run: cargo fmt --all --check
        env: { RUSTUP_TOOLCHAIN: '${{ env.LINT_TOOLCHAIN }}' }

  clippy:
    name: clippy (${{ matrix.features }})
    runs-on: ubuntu-latest
    strategy:
      fail-fast: false
      matrix:
        features: [@MATRIX@]
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@master
        with:
          toolchain: ${{ env.LINT_TOOLCHAIN }}
          components: clippy
      - uses: Swatinem/rust-cache@v2
      - run: cargo clippy --all-targets --locked ${{ matrix.features }} -- -D warnings
        env: { RUSTUP_TOOLCHAIN: '${{ env.LINT_TOOLCHAIN }}' }

  test:
    name: tests (${{ matrix.features }})
    runs-on: ubuntu-latest
    strategy:
      fail-fast: false
      matrix:
        features: ['--no-default-features', '--all-features']
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      - uses: Swatinem/rust-cache@v2
      - run: cargo test --locked ${{ matrix.features }}

  msrv:
    name: MSRV @MSRV@
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@@MSRV_FULL@
      - uses: Swatinem/rust-cache@v2
      - run: cargo build --locked --all-features
        env: { RUSTUP_TOOLCHAIN: '@MSRV_FULL@' }

  zero-deps:
    name: zero third-party dependencies (default build)
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      - name: The default build resolves to first-party code only
        run: |
          set -euo pipefail
          # First party: this crate, and the suite's own crates, which
          # Cargo.lock records with a git source in the suite's organisation.
          first=$( { echo @NAME@; awk -F'"' -v p='@PREFIX@' \
            '/^name = /{n=$2} /^source = /{ if (index($2, p) == 1) print n }' Cargo.lock; } | sort -u)
          tree=$(cargo tree --edges normal --locked --prefix none)
          third=$(printf '%s\n' "$tree" | awk 'NF {print $1}' | sort -u | grep -vxF "$first" || true)
          if [ -n "$third" ]; then
            echo "::error::third-party crates in the default build: $(echo $third)"
            printf '%s\n' "$tree"
            exit 1
          fi
          echo "default build: first-party only ($(echo $first))"

  deny:
    name: cargo-deny (bans, licenses, sources)
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: EmbarkStudios/cargo-deny-action@v2
        with:
          command: check bans licenses sources

  # A RUSTSEC advisory published tomorrow would fail this with no change here;
  # it reports, it does not gate.
  advisories:
    name: cargo-deny (advisories, non-blocking)
    runs-on: ubuntu-latest
    continue-on-error: true
    steps:
      - uses: actions/checkout@v4
      - uses: EmbarkStudios/cargo-deny-action@v2
        with:
          command: check advisories
"#;

const GITWEB_CI: &str = r#"
  # gitweb shells out to `git` and parses what comes back, so the installed git
  # is part of its contract. This caught a real regression: `%aI` renders a
  # zero UTC offset as `+00:00` up to 2.43 and as `Z` after it.
  git-versions:
    name: gitweb vs git ${{ matrix.git }}
    runs-on: ubuntu-latest
    strategy:
      fail-fast: false
      matrix:
        git: ['system', 'newest']
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      - uses: Swatinem/rust-cache@v2
      - name: Install a newer git
        if: matrix.git == 'newest'
        run: |
          sudo add-apt-repository -y ppa:git-core/ppa
          sudo apt-get update -qq && sudo apt-get install -y git
      - run: git --version
      - run: cargo test --locked --all-features
"#;

fn crate_readme(ctx: &Ctx<'_>, name: &str, manifest: &str) -> String {
    let description = manifest_value(manifest, "description").unwrap_or_default();
    let role = if name == LIBRARY {
        "the shared library its engines are built on".to_string()
    } else {
        "one of its engines".to_string()
    };
    let feats = features(manifest);
    let build = if name == LIBRARY {
        "    cargo build                  # the pure library: no third-party crates\n    cargo test --all-features\n".to_string()
    } else {
        format!(
            "    cargo build --release --features net   # the `{name}` binary\n    cargo test --all-features\n"
        )
    };
    format!(
        "# {name}\n\n{description}\n\n\
         Part of the [astrx suite]({suite}): {role}. The suite repository assembles every engine into \
         the single `astrx` binary (`astrx {example}`) and deploys it.\n\n\
         ## Build\n\n{build}\n\
         Features: {feats}. The default build uses the standard library only; `net` (and `rand`) add \
         the suite's two vetted dependencies, tokio and getrandom. CI asserts the default build stays \
         first-party.\n\n\
         ## History\n\n\
         This repository was split out of {suite} with the full history of its files, each followed \
         back through its renames. The cross-check goldens are explained in [GOLDENS.md](GOLDENS.md).\n",
        suite = ctx.url(SUITE),
        example = if name == LIBRARY { "doctor" } else { name },
        feats = if feats.is_empty() { "none".to_string() } else { feats.iter().map(|f| format!("`{f}`")).collect::<Vec<_>>().join(", ") },
    )
}

/// The line every manifest carried about the path dependency on crawlcore.
const COMMENT_OLD: &str =
    "# decorative: every engine depends on crawlcore as `{ path = \"../crawlcore\" }`,";
const COMMENT_NEW: &str =
    "# decorative: the suite's crates depend on each other through their git repositories,";

/// A git dependency on one of the suite's crates. It states the version it
/// expects, which Cargo checks against the crate it fetches: when the
/// dependency's `main` makes a breaking bump, updating fails loudly instead of
/// compiling against whatever `main` now holds.
fn git_dep(ctx: &Ctx<'_>, name: &str) -> String {
    format!(
        "{name} = {{ git = \"{}\", branch = \"main\", version = \"0.1\" }}",
        ctx.url(name)
    )
}

/// One crate's repository (checked out at `dir`): its manifest points at its
/// own repository and at crawlcore's, and it gets the suite's toolchain,
/// format and dependency policy, a README and a CI workflow. The lockfile
/// comes separately ([`crate::lockfile`]).
pub fn crate_part(ctx: &Ctx<'_>, name: &str, dir: &Path) -> Result<Vec<Change>, String> {
    let mut changes: Vec<Change> = Vec::new();
    let mut manifest = read(dir, "Cargo.toml")?;
    let mut edits: Vec<(String, String)> = vec![(
        "repository = \"https://github.com/hydrastro/astrx\"".to_string(),
        format!("repository = \"{}\"", ctx.url(name)),
    )];
    edits.push((COMMENT_OLD.to_string(), COMMENT_NEW.to_string()));
    let mut git_urls = Vec::new();
    if name != LIBRARY {
        edits.push((
            format!("{LIBRARY} = {{ path = \"../{LIBRARY}\" }}"),
            git_dep(ctx, LIBRARY),
        ));
        git_urls.push(ctx.url(LIBRARY));
    }
    let edit_refs: Vec<(&str, &str)> = edits
        .iter()
        .map(|(a, b)| (a.as_str(), b.as_str()))
        .collect();
    manifest = edit("Cargo.toml", manifest, &edit_refs)?;
    // A path dependency that leaves the crate (comments may still mention one).
    if manifest
        .lines()
        .any(|l| !l.trim_start().starts_with('#') && l.contains("path = \"../"))
    {
        return Err(format!(
            "{name}/Cargo.toml still has a path dependency outside the crate"
        ));
    }
    if name == "torrentds" {
        let fuzz = edit(
            "fuzz/Cargo.toml",
            read(dir, "fuzz/Cargo.toml")?,
            &[("path = \"../crates/torrentds\"", "path = \"..\"")],
        )?;
        changes.push(("fuzz/Cargo.toml".into(), Some(fuzz.into_bytes())));
    }
    changes.push((
        "README.md".into(),
        Some(crate_readme(ctx, name, &manifest).into_bytes()),
    ));
    changes.push((
        ".github/workflows/ci.yml".into(),
        Some(crate_ci(ctx, name, &manifest)?.into_bytes()),
    ));
    changes.push(("Cargo.toml".into(), Some(manifest.into_bytes())));
    for f in ["rust-toolchain.toml", "rustfmt.toml"] {
        changes.push((f.into(), Some(ctx.suite_file(f)?.into_bytes())));
    }
    changes.push((
        "deny.toml".into(),
        Some(deny_toml(ctx, &git_urls)?.into_bytes()),
    ));
    changes.push((".gitignore".into(), Some(GITIGNORE.as_bytes().to_vec())));
    Ok(changes)
}

/// The suite repository after the split (checked out at `dir`): the crates
/// that left are gone, the workspace holds only the `astrx` crate, which gets
/// the engines from their repositories, and CI, cargo-deny and the README say
/// so. The lockfile comes separately.
pub fn suite_part(ctx: &Ctx<'_>, dir: &Path) -> Result<Vec<Change>, String> {
    let mut changes: Vec<Change> = Vec::new();
    let all: Vec<&str> = std::iter::once(LIBRARY).chain(ENGINES).collect();
    for c in &all {
        changes.push((format!("crates/{c}"), None));
    }
    for p in ["fuzz", "crates/GOLDENS.md", "tools/split"] {
        changes.push((p.to_string(), None));
    }

    // The workspace: one member.
    let ws = read(dir, "Cargo.toml")?;
    let profile = ws
        .find("[profile.release]")
        .ok_or("Cargo.toml: no [profile.release] — update tools/split/src/adapt.rs")?;
    let repos = all
        .iter()
        .map(|c| format!("#   {c:<13} {}", ctx.url(c)))
        .collect::<Vec<_>>()
        .join("\n");
    let new_ws = format!(
        "# astrx-suite — assembles the suite's engines into the single `astrx` binary\n\
         # and deploys it. Each engine is a crate in a repository of its own, and so is\n\
         # crawlcore, the library they share:\n#\n{repos}\n#\n\
         # crates/astrx gets them as git dependencies; Cargo.lock pins the exact\n\
         # commit of each. To work on an engine and the suite together, point Cargo\n\
         # at a local checkout with a [patch] section, e.g. in .cargo/config.toml:\n\
         #   [patch.'{gitweb}']\n#   gitweb = {{ path = \"../gitweb\" }}\n\
         #\n\
         # Manifests are written WITHOUT Cargo workspace-inheritance, so the workspace\n\
         # parses on old Cargo and a toolchain below the declared `rust-version` fails\n\
         # with a clear MSRV message.\n\
         [workspace]\nresolver = \"2\"\nmembers = [\"crates/astrx\"]\n\n",
        gitweb = ctx.url("gitweb"),
    );
    changes.push((
        "Cargo.toml".into(),
        Some(format!("{new_ws}{}", &ws[profile..]).into_bytes()),
    ));

    // The astrx crate: the engines from their repositories.
    let mut astrx = read(dir, "crates/astrx/Cargo.toml")?;
    let mut edits: Vec<(String, String)> = vec![
        (
            "repository = \"https://github.com/hydrastro/astrx\"".to_string(),
            format!("repository = \"{}\"", ctx.url(SUITE)),
        ),
        (COMMENT_OLD.to_string(), COMMENT_NEW.to_string()),
    ];
    for c in &all {
        edits.push((format!("{c} = {{ path = \"../{c}\" }}"), git_dep(ctx, c)));
    }
    let edit_refs: Vec<(&str, &str)> = edits
        .iter()
        .map(|(a, b)| (a.as_str(), b.as_str()))
        .collect();
    astrx = edit("crates/astrx/Cargo.toml", astrx, &edit_refs)?;
    changes.push(("crates/astrx/Cargo.toml".into(), Some(astrx.into_bytes())));

    let urls: Vec<String> = all.iter().map(|c| ctx.url(c)).collect();
    changes.push((
        "deny.toml".into(),
        Some(deny_toml(ctx, &urls)?.into_bytes()),
    ));

    // CI: the gitweb/git matrix moved to gitweb's repository; the zero-deps
    // gate counts the suite's own git crates as first party.
    let ci_path = ".github/workflows/ci.yml";
    let mut ci = read(dir, ci_path)?;
    let start = ci
        .find("  # ---------------------------------- git's output is not a stable format\n")
        .ok_or("ci.yml: the git-versions job's marker moved — update tools/split/src/adapt.rs")?;
    let end = ci
        .find("  # ------------------------------------------------ the shipped installer itself\n")
        .ok_or("ci.yml: the installer job's marker moved — update tools/split/src/adapt.rs")?;
    if end < start {
        return Err("ci.yml: job markers out of order — update tools/split/src/adapt.rs".into());
    }
    ci.replace_range(start..end, "");
    let first_party = format!(
        "          echo \"workspace members: $(echo \"$members\" | tr '\\n' ' ')\"\n\
         \x20         # The engines and crawlcore are git dependencies now, from the\n\
         \x20         # suite's own repositories: first party too.\n\
         \x20         org=$(awk -F'\"' -v p='git+{prefix}' \\\n\
         \x20           '/^name = /{{n=$2}} /^source = /{{ if (index($2, p) == 1) print n }}' Cargo.lock)\n\
         \x20         members=$(printf '%s\\n%s\\n' \"$members\" \"$org\" | sed '/^$/d' | sort -u)\n",
        prefix = ctx.url_prefix(),
    );
    ci = edit(
        ci_path,
        ci,
        &[(
            "          echo \"workspace members: $(echo \"$members\" | tr '\\n' ' ')\"\n",
            &first_party,
        )],
    )?;
    changes.push((ci_path.into(), Some(ci.into_bytes())));

    // README: where the crates went.
    let readme = read(dir, "README.md")?;
    let list = all
        .iter()
        .map(|c| format!("| `{c}` | {} |", ctx.url(c)))
        .collect::<Vec<_>>()
        .join("\n");
    let readme = edit(
        "README.md",
        readme,
        &[
            (
                "A Cargo\nworkspace; each engine is a crate; `crawlcore` is the shared library.",
                "Each engine\nis a crate in a repository of its own, and so is `crawlcore`, the library they\nshare; this repository assembles them into the single `astrx` binary and\ndeploys it.",
            ),
            (
                "## Layout\n",
                &format!(
                    "## Repositories\n\n| Crate | Repository |\n|-------|------------|\n{list}\n\n\
                     `crates/astrx` gets each as a git dependency, pinned to an exact commit by\n\
                     `Cargo.lock`. Each crate's own CI runs its tests, lints, MSRV build and\n\
                     zero-dependency check; this repository's CI checks the assembled binary.\n\n\
                     ## Layout\n"
                ),
            ),
            (
                "├── crates/\n│   ├── crawlcore/        shared crawl library  ✅\n│   ├── torrentds/        DHT indexer + tracker (node, trackers, metadata) ✅\n│   ├── onioncrawler/     darknet (.onion/.i2p) crawler — engine complete ✅\n│   ├── websearch/        clearnet search engine — feature-complete ✅\n│   ├── suitedash/        no-JS ops/status dashboard ✅\n│   └── gitweb/           read-only no-JS git frontend ✅\n",
                "├── crates/\n│   └── astrx/            the single binary: every engine, `doctor`, `run`\n",
            ),
            ("├── docker-compose.yml    one-command Tor deployment (no host ports)\n└── fuzz/                 cargo-fuzz harnesses for the wire parsers\n",
             "└── docker-compose.yml    one-command Tor deployment (no host ports)\n"),
            ("(see\n`crates/GOLDENS.md`)", "(see each\nengine's `GOLDENS.md`)"),
            ("harnesses under `fuzz/`.", "harnesses in the `torrentds` repository's `fuzz/`."),
        ],
    )?;
    changes.push(("README.md".into(), Some(readme.into_bytes())));
    Ok(changes)
}
