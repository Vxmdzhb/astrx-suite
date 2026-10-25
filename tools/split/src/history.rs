//! Rewriting history so it holds only some files, under new names.
//!
//! `git subtree split` follows one directory, so a crate's history would
//! begin wherever its directory last moved. This follows each file of a part
//! back through its renames instead: the history reaches the file's first
//! commit, wherever it lived then. A former name counts only in the commits
//! before the rename (the rename commit's ancestors), so a file something
//! else later re-created at that old path is not swept in. Commits that end
//! up changing nothing in the part are dropped.
//!
//! Done with git itself: `fast-export --no-data` (so blobs stay where they
//! are), a filter here, `fast-import` into the same repository. Rewritten
//! commits keep author, committer, dates and message; their signatures cannot
//! survive (the signed content changed) — [`sign`] re-signs them.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::Path;

use crate::git::{git, git_str, git_with};

/// A rename git detects: (commit, old path, new path).
pub type Rename = (String, String, String);

/// Every rename in `rev`'s history, newest first, at similarity ≥ `min` %
/// (so two small unrelated files are not taken for one).
pub fn renames(repo: &Path, rev: &str, min: u8) -> Result<Vec<Rename>, String> {
    let m = format!("-M{min}%");
    let log = git(
        repo,
        &[
            "-c",
            "core.quotePath=false",
            "log",
            "-z",
            &m,
            "--diff-filter=R",
            "--name-status",
            "--format=%x01%H",
            rev,
        ],
    )?;
    let log = String::from_utf8(log).map_err(|_| "git log: a path is not UTF-8".to_string())?;
    let mut out = Vec::new();
    for chunk in log.split('\u{1}') {
        if chunk.is_empty() {
            continue;
        }
        let mut fields = chunk.split('\0');
        let commit = fields.next().unwrap_or("").trim().to_string();
        let rest: Vec<&str> = fields.collect();
        let mut i = 0;
        // -z: status, old, new, status, old, new, …
        while i + 2 < rest.len() {
            let status = rest[i].trim();
            if status.starts_with('R') {
                out.push((
                    commit.clone(),
                    rest[i + 1].to_string(),
                    rest[i + 2].to_string(),
                ));
            }
            i += 3;
        }
    }
    Ok(out)
}

/// Which paths a part keeps, and under which names.
pub struct Mapper<'a> {
    repo: &'a Path,
    /// (source, destination): a directory ends with '/', otherwise one file.
    sources: Vec<(String, String)>,
    /// Former name → [(new path, renamed in commit)].
    former: HashMap<String, Vec<(String, String)>>,
    ancestors: RefCell<HashMap<String, HashSet<String>>>,
}

impl<'a> Mapper<'a> {
    /// The files under each source at `rev`, renamed to their destination,
    /// plus every former name of those files (before it was renamed).
    pub fn new(
        repo: &'a Path,
        rev: &str,
        sources: &[(&str, &str)],
        renames: &[Rename],
    ) -> Result<Mapper<'a>, String> {
        let mut names: HashMap<String, String> = HashMap::new();
        for (src, dst) in sources {
            let listed = git(
                repo,
                &["ls-tree", "-r", "-z", "--name-only", rev, "--", src],
            )?;
            for p in String::from_utf8_lossy(&listed)
                .split('\0')
                .filter(|p| !p.is_empty())
            {
                let to = if src.ends_with('/') {
                    format!("{dst}{}", &p[src.len()..])
                } else {
                    (*dst).to_string()
                };
                names.insert(p.to_string(), to);
            }
        }
        if names.is_empty() {
            return Err(format!("nothing at {rev} under {sources:?}"));
        }
        let mut former: HashMap<String, Vec<(String, String)>> = HashMap::new();
        for (commit, old, new) in renames {
            if let Some(to) = names.get(new).cloned() {
                former
                    .entry(old.clone())
                    .or_default()
                    .push((to.clone(), commit.clone()));
                names.insert(old.clone(), to);
            }
        }
        Ok(Mapper {
            repo,
            sources: sources
                .iter()
                .map(|(a, b)| ((*a).to_string(), (*b).to_string()))
                .collect(),
            former,
            ancestors: RefCell::new(HashMap::new()),
        })
    }

    fn is_before(&self, commit: &str, renamed_in: &str) -> bool {
        let mut cache = self.ancestors.borrow_mut();
        if !cache.contains_key(renamed_in) {
            let parents = format!("{renamed_in}^@");
            let list = git_str(self.repo, &["rev-list", &parents, "--"]).unwrap_or_default();
            cache.insert(
                renamed_in.to_string(),
                list.lines().map(str::to_string).collect(),
            );
        }
        cache[renamed_in].contains(commit)
    }

    /// The new name of `path` in `commit`, or `None` when the part does not
    /// keep it.
    pub fn map(&self, path: &str, commit: &str) -> Option<String> {
        for (src, dst) in &self.sources {
            if src.ends_with('/') {
                if let Some(rest) = path.strip_prefix(src.as_str()) {
                    return Some(format!("{dst}{rest}"));
                }
            } else if path == src {
                return Some(dst.clone());
            }
        }
        for (to, renamed_in) in self.former.get(path)? {
            if self.is_before(commit, renamed_in) {
                return Some(to.clone());
            }
        }
        None
    }
}

/// Undo fast-export's C-style quoting of a path.
fn unquote(p: &[u8]) -> Vec<u8> {
    if p.first() != Some(&b'"') || p.len() < 2 {
        return p.to_vec();
    }
    let inner = &p[1..p.len() - 1];
    let mut out = Vec::with_capacity(inner.len());
    let mut i = 0;
    while i < inner.len() {
        let c = inner[i];
        if c != b'\\' || i + 1 >= inner.len() {
            out.push(c);
            i += 1;
            continue;
        }
        let n = inner[i + 1];
        match n {
            b'n' => out.push(b'\n'),
            b't' => out.push(b'\t'),
            b'"' => out.push(b'"'),
            b'\\' => out.push(b'\\'),
            b'a' => out.push(7),
            b'b' => out.push(8),
            b'f' => out.push(12),
            b'r' => out.push(b'\r'),
            b'v' => out.push(11),
            // \ooo: exactly three octal digits (git writes non-ASCII bytes so).
            b'0'..=b'3' if i + 3 < inner.len() => {
                let oct = std::str::from_utf8(&inner[i + 1..i + 4]).unwrap_or("");
                match u8::from_str_radix(oct, 8) {
                    Ok(byte) => out.push(byte),
                    Err(_) => out.extend_from_slice(&inner[i..i + 4]),
                }
                i += 4;
                continue;
            }
            other => out.push(other),
        }
        i += 2;
    }
    out
}

/// Quote a path for fast-import when it has to be.
fn quote(p: &str) -> String {
    if !p.contains(['"', '\\', '\n']) && !p.starts_with('"') {
        return p.to_string();
    }
    let mut s = String::from("\"");
    for c in p.chars() {
        match c {
            '"' => s.push_str("\\\""),
            '\\' => s.push_str("\\\\"),
            '\n' => s.push_str("\\n"),
            c => s.push(c),
        }
    }
    s.push('"');
    s
}

/// Rewrite `rev`'s history keeping only what `mapper` keeps, onto `refname`
/// in the same repository (which has every blob already). Returns the new
/// tip's id.
pub fn rewrite(
    repo: &Path,
    rev: &str,
    mapper: &Mapper<'_>,
    refname: &str,
) -> Result<String, String> {
    let stream = git(
        repo,
        &[
            "-c",
            "core.quotePath=false",
            "fast-export",
            "--no-data",
            "--show-original-ids",
            "--reencode=yes",
            "--signed-tags=strip",
            "--tag-of-filtered-object=drop",
            rev,
        ],
    )?;
    let mut out: Vec<u8> = Vec::new();
    let mut alias: HashMap<String, String> = HashMap::new(); // dropped mark → stands for ("" = none)
    let resolve = |alias: &HashMap<String, String>, mut m: String| -> String {
        while let Some(next) = alias.get(&m) {
            m = next.clone();
        }
        m
    };
    let mut pos = 0usize;
    let next_line = |pos: &mut usize| -> Option<Vec<u8>> {
        if *pos >= stream.len() {
            return None;
        }
        let end = stream[*pos..]
            .iter()
            .position(|&b| b == b'\n')
            .map_or(stream.len(), |i| *pos + i);
        let line = stream[*pos..end].to_vec();
        *pos = end + 1;
        Some(line)
    };
    let mut tip = String::new();

    while let Some(line) = next_line(&mut pos) {
        if !line.starts_with(b"commit ") {
            continue; // reset / feature / done: refs renamed below, the rest dropped
        }
        let mut mark = String::new();
        let mut orig = String::new();
        let mut head: Vec<Vec<u8>> = Vec::new();
        let mut from: Option<String> = None;
        let mut merges: Vec<String> = Vec::new();
        let mut changes: Vec<String> = Vec::new();
        while let Some(l) = next_line(&mut pos) {
            if l.is_empty() {
                break;
            }
            let text = String::from_utf8_lossy(&l).to_string();
            if let Some(m) = text.strip_prefix("mark ") {
                mark = m.to_string();
            } else if let Some(o) = text.strip_prefix("original-oid ") {
                orig = o.to_string();
            } else if text.starts_with("author ")
                || text.starts_with("committer ")
                || text.starts_with("encoding ")
            {
                head.push(l.clone());
            } else if let Some(n) = text.strip_prefix("data ") {
                let n: usize = n
                    .trim()
                    .parse()
                    .map_err(|_| format!("bad data length: {text}"))?;
                let mut block = l.clone();
                block.push(b'\n');
                block.extend_from_slice(&stream[pos..pos + n]);
                head.push(block);
                pos += n;
                if pos < stream.len() && stream[pos] == b'\n' {
                    pos += 1;
                }
            } else if let Some(f) = text.strip_prefix("from ") {
                from = Some(f.to_string());
            } else if let Some(m) = text.strip_prefix("merge ") {
                merges.push(m.to_string());
            } else if l.starts_with(b"M ") {
                // M <mode> <dataref> <path>
                let mut parts = l.splitn(4, |&b| b == b' ');
                let _ = parts.next();
                let mode = String::from_utf8_lossy(parts.next().unwrap_or(b"")).to_string();
                let dataref = String::from_utf8_lossy(parts.next().unwrap_or(b"")).to_string();
                let path = unquote(parts.next().unwrap_or(b""));
                let path =
                    String::from_utf8(path).map_err(|_| "a path is not UTF-8".to_string())?;
                if let Some(to) = mapper.map(&path, &orig).filter(|t| !t.is_empty()) {
                    changes.push(format!("M {mode} {dataref} {}", quote(&to)));
                }
            } else if l.starts_with(b"D ") {
                let path = String::from_utf8(unquote(&l[2..]))
                    .map_err(|_| "a path is not UTF-8".to_string())?;
                if let Some(to) = mapper.map(&path, &orig).filter(|t| !t.is_empty()) {
                    changes.push(format!("D {}", quote(&to)));
                }
            } else if text == "deleteall" {
                changes.push("deleteall".to_string());
            }
            // gpgsig and anything else: dropped (a signature over rewritten content is void).
        }
        tip = mark.clone(); // topological order: `rev` itself comes last

        let mut parents: Vec<String> = Vec::new();
        for p in from.into_iter().chain(merges) {
            let r = resolve(&alias, p);
            if !r.is_empty() && !parents.contains(&r) {
                parents.push(r);
            }
        }
        if changes.is_empty() && parents.len() <= 1 {
            alias.insert(mark, parents.first().cloned().unwrap_or_default());
            continue;
        }
        if parents.is_empty() {
            out.extend_from_slice(format!("reset {refname}\n").as_bytes());
        }
        out.extend_from_slice(format!("commit {refname}\nmark {mark}\n").as_bytes());
        for h in &head {
            out.extend_from_slice(h);
            out.push(b'\n');
        }
        if let Some((first, rest)) = parents.split_first() {
            out.extend_from_slice(format!("from {first}\n").as_bytes());
            for p in rest {
                out.extend_from_slice(format!("merge {p}\n").as_bytes());
            }
        }
        for c in &changes {
            out.extend_from_slice(c.as_bytes());
            out.push(b'\n');
        }
        out.push(b'\n');
    }

    let last = if tip.is_empty() {
        String::new()
    } else {
        resolve(&alias, tip)
    };
    if out.is_empty() || last.is_empty() {
        return Err(format!("nothing of {rev} is kept"));
    }
    // The ref ends at `rev`'s counterpart, even when `rev` itself changed
    // nothing kept here and so is not the last commit written.
    out.extend_from_slice(format!("reset {refname}\nfrom {last}\n\ndone\n").as_bytes());
    git_with(
        repo,
        &["fast-import", "--quiet", "--force", "--done"],
        &[],
        Some(&out),
    )?;
    git_str(repo, &["rev-parse", refname])
}

/// Sign the commits of `branch` (those after `base`, or all of them): re-create
/// each one, oldest first, with `git commit-tree -S` — same tree, same
/// (re-created) parents, same author, committer, dates and message — and move
/// the branch to the result. Unlike `git rebase --exec 'git commit --amend
/// -S'`, the history keeps its exact shape (roots, merges). Uses the
/// repository's signing configuration.
pub fn sign(repo: &Path, branch: &str, base: Option<&str>) -> Result<String, String> {
    let full = format!("refs/heads/{branch}");
    let range = match base {
        Some(b) => format!("{b}..{full}"),
        None => full.clone(),
    };
    let list = git_str(
        repo,
        &["rev-list", "--reverse", "--topo-order", "--parents", &range],
    )?;
    let mut map: HashMap<String, String> = HashMap::new();
    for row in list.lines() {
        let mut ids = row.split(' ');
        let commit = ids.next().unwrap_or("").to_string();
        let raw = git(repo, &["cat-file", "commit", &commit])?;
        let split = raw
            .windows(2)
            .position(|w| w == b"\n\n")
            .ok_or_else(|| format!("{commit}: malformed"))?;
        let header = String::from_utf8_lossy(&raw[..split]).to_string();
        let message = &raw[split + 2..];
        let mut tree = String::new();
        let mut encoding: Option<String> = None;
        let mut env: Vec<(String, String)> = Vec::new();
        for h in header.lines() {
            if let Some(t) = h.strip_prefix("tree ") {
                tree = t.to_string();
            } else if let Some(e) = h.strip_prefix("encoding ") {
                encoding = Some(e.to_string());
            } else if let Some((who, rest)) = h
                .split_once(' ')
                .filter(|(w, _)| *w == "author" || *w == "committer")
            {
                // Name <email> 1700000000 +0100
                let lt = rest
                    .rfind(" <")
                    .ok_or_else(|| format!("{commit}: bad {who}"))?;
                let gt = rest
                    .rfind("> ")
                    .ok_or_else(|| format!("{commit}: bad {who}"))?;
                let w = who.to_uppercase();
                env.push((format!("GIT_{w}_NAME"), rest[..lt].to_string()));
                env.push((format!("GIT_{w}_EMAIL"), rest[lt + 2..gt].to_string()));
                env.push((format!("GIT_{w}_DATE"), format!("@{}", &rest[gt + 2..])));
            }
        }
        if tree.is_empty() || env.len() != 6 {
            return Err(format!(
                "{commit}: could not read its tree, author or committer"
            ));
        }
        let mut args: Vec<String> = Vec::new();
        if let Some(e) = &encoding {
            args.push("-c".into());
            args.push(format!("i18n.commitEncoding={e}"));
        }
        args.push("commit-tree".into());
        args.push(tree);
        args.push("-S".into());
        for p in ids {
            args.push("-p".into());
            args.push(map.get(p).cloned().unwrap_or_else(|| p.to_string()));
        }
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let env_refs: Vec<(&str, &str)> =
            env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        let new = git_with(repo, &arg_refs, &env_refs, Some(message))?;
        map.insert(commit, String::from_utf8_lossy(&new).trim().to_string());
    }
    let tip_old = git_str(repo, &["rev-parse", &full])?;
    let tip = map
        .get(&tip_old)
        .cloned()
        .ok_or_else(|| format!("{branch}: nothing to sign"))?;
    git(repo, &["update-ref", &full, &tip])?;
    Ok(tip)
}

#[cfg(test)]
mod tests {
    use super::{quote, unquote};

    #[test]
    fn paths_round_trip_through_fast_export_quoting() {
        assert_eq!(unquote(b"plain/path.rs"), b"plain/path.rs");
        assert_eq!(unquote(b"\"a\\\"b\\\\c\\nd\""), b"a\"b\\c\nd");
        assert_eq!(unquote(b"\"caf\\303\\251\""), "café".as_bytes());
        assert_eq!(quote("plain"), "plain");
        assert_eq!(quote("a\"b"), "\"a\\\"b\"");
        let tricky = "x\"y\\z\nw";
        assert_eq!(unquote(quote(tricky).as_bytes()), tricky.as_bytes());
    }
}
