//! The kind of project the active tab's dir holds, and the command lines the
//! project bar's buttons run (see `ui::project_bar`). Git is
//! told by the tab's branch instead (see `State::branch`), as for the prompt;
//! the repo's other branches, for the Git group, are read here too (see
//! [Branches]).

use std::{
    env, fs,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
};

use shell::{quiet_command, venv_python};

/// Entry-point names Python's Run looks for, in this order, matched without
/// regard to case.
const ENTRY_POINTS: &[&str] = &["run.py", "main.py", "app.py"];

/// What the files in a dir say it's a project of.
#[derive(Clone, Copy, Default, PartialEq)]
pub struct Project {
    /// Holds a `Cargo.toml`.
    pub cargo: bool,
    /// Holds a `pyproject.toml`, `setup.py` or Django `manage.py`, or a venv.
    pub python: bool,
}

impl Project {
    pub fn detect(dir: &Path) -> Self {
        let has = |name: &str| dir.join(name).is_file();
        Self {
            cargo: has("Cargo.toml"),
            python: ["pyproject.toml", "setup.py", "manage.py"]
                .into_iter()
                .any(has)
                || venv_python(dir).is_some(),
        }
    }
}

/// The line Git's Branch runs in `dir` for the branch `name`: switch to it if
/// it's there, locally or on a remote (which `git switch` makes a tracking
/// branch of), else create it. Names are kept to characters that need no
/// quoting, since the line goes to the system shell.
pub fn git_branch(dir: &Path, name: &str) -> Result<String, String> {
    let name = name.trim();
    let safe = name
        .chars()
        .all(|c| c.is_alphanumeric() || "._/-+".contains(c));
    if name.is_empty() || name.starts_with('-') || !safe {
        return Err(format!(
            "branch: `{name}` isn't a branch name this button takes \
             (letters, digits and . _ / - +, not starting with -)"
        ));
    }

    let git = |args: &[&str]| {
        quiet_command("git")
            .args(args)
            .current_dir(dir)
            .output()
            .ok()
    };
    let local = git(&[
        "show-ref",
        "--verify",
        "--quiet",
        &format!("refs/heads/{name}"),
    ])
    .is_some_and(|o| o.status.success());
    let remote = || {
        git(&["branch", "-r", "--list", &format!("*/{name}")])
            .is_some_and(|o| o.status.success() && !o.stdout.trim_ascii().is_empty())
    };

    Ok(if local || remote() {
        format!("git switch {name}")
    } else {
        format!("git switch -c {name}")
    })
}

/// The line the Projects window's Clone runs: clone `url` to `dest`.
pub fn git_clone(url: &str, dest: &Path) -> String {
    format!(
        "git clone {} {}",
        quote(url),
        quote(&dest.to_string_lossy())
    )
}

/// The line the Projects window's Pull runs, in the repo at `dir`.
pub fn git_pull(dir: &Path) -> String {
    format!("git -C {} pull", quote(&dir.to_string_lossy()))
}

/// How recently a branch must have been committed to or checked out for the
/// Git group to give it a button (see [Branches::recent]).
const RECENT_BRANCH_DAYS: i64 = 7;

/// Of the [Branches::recent] ones, the most the Git group gives buttons, so
/// it can't run off the window.
pub const MAX_RECENT_BRANCHES: usize = 8;

/// How much of the end of `HEAD`'s reflog is read for its latest checkouts:
/// plenty for a week's, however long the log has grown. A branch's own needs
/// only its last line, so a little of it is read.
const HEAD_REFLOG_TAIL: u64 = 64 * 1024;
const BRANCH_REFLOG_TAIL: u64 = 4 * 1024;

/// The branches of the repo a dir is in, for the Git group's Branch box to
/// complete and its branch buttons. Read straight from the repo's files, not
/// by running `git`, so it's cheap to read again whenever the branch might
/// have changed (see `State::git_branches`).
#[derive(Default)]
pub struct Branches {
    /// The dir they were read for.
    pub dir: PathBuf,
    /// The repo's top dir; `None` when `dir`'s in none.
    pub root: Option<PathBuf>,
    /// Every local branch, and every remote one by the name `git switch`
    /// takes for it (`feature` for `origin/feature`): sorted, each once.
    pub names: Vec<String>,
    /// The local branches committed to, created or checked out in the last
    /// [RECENT_BRANCH_DAYS] days, most recent first.
    pub recent: Vec<String>,
}

impl Branches {
    /// The branches of the repo `dir` is in; none when it's in none.
    pub fn read(dir: &Path) -> Self {
        let mut branches = Self {
            dir: dir.to_path_buf(),
            ..Default::default()
        };
        let Some(repo) = GitDirs::find(dir) else {
            return branches;
        };

        let local = ref_names(&repo.common, "refs/heads");
        // `<remote>/<branch>`, but not the remote's `HEAD`.
        let remote = ref_names(&repo.common, "refs/remotes")
            .into_iter()
            .filter_map(|r| r.split_once('/').map(|(_, name)| name.to_owned()))
            .filter(|name| name != "HEAD");

        branches.root = Some(repo.root.clone());
        branches.recent = recent_branches(&repo, &local);
        branches.names = local.into_iter().chain(remote).collect();
        branches.names.sort();
        branches.names.dedup();
        branches
    }
}

/// Where a repo keeps its files.
struct GitDirs {
    /// The worktree's top dir, holding `.git`.
    root: PathBuf,
    /// This worktree's own: its `HEAD`, and that's reflog.
    git: PathBuf,
    /// Those its worktrees share: the refs, and their reflogs. `git`, but
    /// for a linked worktree.
    common: PathBuf,
}

impl GitDirs {
    /// The repo `dir` is in, by the `.git` in it or the nearest dir above.
    fn find(dir: &Path) -> Option<Self> {
        let dot = dir
            .ancestors()
            .map(|d| d.join(".git"))
            .find(|p| p.exists())?;
        let root = dot.parent()?.to_path_buf();
        let git = if dot.is_dir() {
            dot
        } else {
            // A linked worktree's or submodule's: `gitdir: <path>`, from the
            // dir it's in.
            let text = fs::read_to_string(&dot).ok()?;
            let path = text.trim().strip_prefix("gitdir:")?.trim();
            root.join(path)
        };
        let common = match fs::read_to_string(git.join("commondir")) {
            Ok(text) => git.join(text.trim()),
            Err(_) => git.clone(),
        };
        Some(Self { root, git, common })
    }
}

/// The names of the refs under `prefix` (e.g. `refs/heads`) in the repo
/// whose shared files are in `common`, without it: loose ones, each a file
/// under that dir, and packed ones, listed in `packed-refs`. Sorted, each
/// once.
fn ref_names(common: &Path, prefix: &str) -> Vec<String> {
    let mut names = Vec::new();
    loose_refs(&common.join(prefix), "", &mut names);

    if let Ok(packed) = fs::read_to_string(common.join("packed-refs")) {
        let prefix = format!("{prefix}/");
        // `<sha> <ref>`, after a `# pack-refs` header. Lines starting `^`
        // are the commit an annotated tag on the line before points to.
        names.extend(
            packed
                .lines()
                .filter_map(|line| line.split_once(' '))
                .filter_map(|(_, r)| r.strip_prefix(&prefix))
                .map(str::to_owned),
        );
    }
    names.sort();
    names.dedup();
    names
}

/// Add the refs in `dir`, and the dirs in it, to `names`, as `<within>name`.
fn loose_refs(dir: &Path, within: &str, names: &mut Vec<String>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = format!("{within}{}", entry.file_name().to_string_lossy());
        match entry.file_type() {
            Ok(t) if t.is_dir() => loose_refs(&entry.path(), &format!("{name}/"), names),
            // Not one git's in the middle of writing.
            Ok(_) if !name.ends_with(".lock") => names.push(name),
            _ => {}
        }
    }
}

/// Those of the `local` branches committed to, created or checked out in the
/// last [RECENT_BRANCH_DAYS] days, most recent first: by the last entry in
/// each one's reflog, and the checkouts in `HEAD`'s.
fn recent_branches(repo: &GitDirs, local: &[String]) -> Vec<String> {
    let now = chrono::Utc::now().timestamp();
    let since = now - RECENT_BRANCH_DAYS * 24 * 60 * 60;

    let logs = repo.common.join("logs").join("refs").join("heads");
    let mut last: Vec<(i64, &String)> = local
        .iter()
        .filter_map(|b| {
            let lines = reflog_tail(&logs.join(b), BRANCH_REFLOG_TAIL);
            let time = lines.lines().rev().find_map(reflog_entry)?.0;
            Some((time, b))
        })
        .collect();

    // `git checkout` and `git switch` both log "checkout: moving from <a> to
    // <b>".
    let head = reflog_tail(&repo.git.join("logs").join("HEAD"), HEAD_REFLOG_TAIL);
    for (time, msg) in head.lines().filter_map(reflog_entry) {
        let Some(to) = msg
            .strip_prefix("checkout: moving from ")
            .and_then(|m| m.rsplit_once(" to "))
            .map(|(_, to)| to)
        else {
            continue;
        };
        let Some(b) = local.iter().find(|b| *b == to) else {
            continue;
        };
        match last.iter_mut().find(|(_, name)| *name == b) {
            Some(entry) => entry.0 = entry.0.max(time),
            None => last.push((time, b)),
        }
    }

    last.retain(|(time, _)| *time >= since);
    last.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(b.1)));
    last.into_iter().map(|(_, b)| b.clone()).collect()
}

/// The last `max` bytes of the reflog at `path`, from the first whole line
/// in them. Empty if there's none.
fn reflog_tail(path: &Path, max: u64) -> String {
    let Ok(mut file) = fs::File::open(path) else {
        return String::new();
    };
    let len = file.metadata().map_or(0, |m| m.len());
    let start = len.saturating_sub(max);
    let mut bytes = Vec::new();
    if file.seek(SeekFrom::Start(start)).is_err() || file.read_to_end(&mut bytes).is_err() {
        return String::new();
    }
    let text = String::from_utf8_lossy(&bytes);
    // Begun mid-line, unless at the start.
    match text.split_once('\n') {
        Some((_, rest)) if start > 0 => rest.to_owned(),
        _ => text.into_owned(),
    }
}

/// The time (in Unix seconds) and message of a reflog line:
/// `<old sha> <new sha> <name> <<email>> <time> <zone>\t<message>`.
fn reflog_entry(line: &str) -> Option<(i64, &str)> {
    let (who, msg) = line.split_once('\t').unwrap_or((line, ""));
    let when = &who[who.rfind('>')? + 1..];
    let time = when.split_whitespace().next()?.parse().ok()?;
    Some((time, msg))
}

/// The line Python's Run runs in `dir`: Django's dev server when there's a
/// `manage.py`, otherwise the first [ENTRY_POINTS] file found by
/// [entry_point]. Found when clicked, so nothing's searched until then.
pub fn python_run(dir: &Path) -> Result<String, String> {
    let python = if use_uv(dir) {
        "uv run python"
    } else {
        "python"
    };
    if find_entry(dir, "manage.py", true).is_some() {
        return Ok(format!("{python} manage.py runserver"));
    }
    match entry_point(dir) {
        Some(args) => Ok(format!("{python} {args}")),
        None => Err(format!(
            "run: no manage.py, nor {} in this folder, `src`, or the project's package folder",
            ENTRY_POINTS.join(", ")
        )),
    }
}

/// The line Python's Black runs in `dir`, formatting every Python file under
/// it (Black skips venvs and .gitignored files itself). The venv's Black
/// when it has one, so the project's pinned version is used; otherwise uv's
/// (`uvx`) or the one on PATH.
pub fn python_black(dir: &Path) -> &'static str {
    let venv_black = venv_python(dir)
        .and_then(|py| py.parent().map(Path::to_path_buf))
        .is_some_and(|bin| ["black.exe", "black"].iter().any(|b| bin.join(b).is_file()));

    match (use_uv(dir), venv_black) {
        (true, true) => "uv run black .",
        (true, false) => "uvx black .",
        // `python` is the venv's (see `shell::commands::expand_input`).
        (false, true) => "python -m black .",
        (false, false) => "black .",
    }
}

/// Whether Python's buttons go through uv: it's on PATH, and `dir` is a
/// project of the kind it manages. Without a `pyproject.toml`, `uv run`
/// wouldn't find a `venv` (only `.venv`), so the venv's own python is used.
fn use_uv(dir: &Path) -> bool {
    dir.join("pyproject.toml").is_file() && on_path("uv")
}

fn on_path(program: &str) -> bool {
    let exe = format!("{program}{}", env::consts::EXE_SUFFIX);
    env::var_os("PATH")
        .is_some_and(|paths| env::split_paths(&paths).any(|d| d.join(&exe).is_file()))
}

/// The arguments to `python` for the first [ENTRY_POINTS] file in the
/// project at `root`, looking in the root, `src`, the package folder (named
/// for the project), then `src/<package>`. One in the package folder runs as
/// a module (`-m pkg.main`), so its imports of the package resolve; the
/// rest run by path.
fn entry_point(root: &Path) -> Option<String> {
    let mut dirs: Vec<(PathBuf, Option<String>)> = vec![(root.to_path_buf(), None)];
    let src = find_entry(root, "src", false);
    if let Some(src) = &src {
        dirs.push((root.join(src), None));
    }
    for name in package_names(root) {
        if let Some(pkg) = find_entry(root, &name, false) {
            dirs.push((root.join(&pkg), Some(pkg)));
        }
        if let Some(src) = &src
            && let Some(pkg) = find_entry(&root.join(src), &name, false)
        {
            dirs.push((root.join(src).join(pkg), None));
        }
    }

    dirs.into_iter().find_map(|(dir, module)| {
        let file = ENTRY_POINTS
            .iter()
            .find_map(|e| find_entry(&dir, e, true))?;
        Some(match module {
            Some(pkg) => {
                let stem = file.strip_suffix(".py").unwrap_or(&file).to_owned();
                format!("-m {pkg}.{}", stem)
            }
            None => {
                let rel = dir.strip_prefix(root).unwrap_or(&dir).join(&file);
                quote(&rel.to_string_lossy().replace('\\', "/"))
            }
        })
    })
}

/// The project's possible package names: its `pyproject.toml` name, and the
/// root folder's, as an import would spell them.
fn package_names(root: &Path) -> Vec<String> {
    let mut names: Vec<String> = [
        pyproject_name(root),
        root.file_name().map(|n| n.to_string_lossy().into_owned()),
    ]
    .into_iter()
    .flatten()
    .map(|n| n.replace(['-', '.', ' '], "_"))
    .collect();
    names.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
    names
}

/// `name` from `pyproject.toml`'s `[project]` (or Poetry's `[tool.poetry]`)
/// table. Read by hand: it's the one value needed.
fn pyproject_name(root: &Path) -> Option<String> {
    let text = fs::read_to_string(root.join("pyproject.toml")).ok()?;
    let mut in_table = false;
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') {
            in_table = line == "[project]" || line == "[tool.poetry]";
            continue;
        }
        let value = line
            .strip_prefix("name")
            .and_then(|rest| rest.trim_start().strip_prefix('='))
            .map(str::trim);
        let Some(value) = value.filter(|_| in_table) else {
            continue;
        };
        let q = value.chars().next().filter(|c| *c == '"' || *c == '\'')?;
        let name = value[1..].split(q).next()?;
        if !name.is_empty() {
            return Some(name.to_owned());
        }
    }
    None
}

/// The name of the file (or with `file` false, folder) in `dir` called
/// `name`, ignoring case, as it's spelled there.
fn find_entry(dir: &Path, name: &str, file: bool) -> Option<String> {
    fs::read_dir(dir).ok()?.flatten().find_map(|e| {
        let n = e.file_name().to_string_lossy().into_owned();
        let kind_ok = e
            .file_type()
            .is_ok_and(|t| if file { t.is_file() } else { t.is_dir() });
        (kind_ok && n.eq_ignore_ascii_case(name)).then_some(n)
    })
}

/// `s` single-quoted for the system shell (pwsh or sh alike) when it has
/// anything but plain path characters.
fn quote(s: &str) -> String {
    if s.chars().all(|c| c.is_alphanumeric() || "._/-".contains(c)) {
        s.to_owned()
    } else {
        format!("'{}'", s.replace('\'', "''"))
    }
}

#[cfg(test)]
mod tests {
    use shell::project_root;

    use super::*;

    #[test]
    fn project_roots_use_the_nearest_git_cargo_or_python_marker() {
        let root = env::temp_dir().join(format!(
            "peregrine_project_roots_{}_{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap()
        ));
        fs::create_dir(&root).unwrap();
        assert_eq!(project_root(&root), None);

        fs::create_dir(root.join(".git")).unwrap();
        let plain = root.join("plain/sub");
        fs::create_dir_all(&plain).unwrap();
        assert_eq!(project_root(&plain), Some(root.clone()));
        assert_eq!(project_root(&root), Some(root.clone()));

        for (i, marker) in [
            ".git",
            "Cargo.toml",
            "pyproject.toml",
            "setup.py",
            "manage.py",
        ]
        .into_iter()
        .enumerate()
        {
            let nested = root.join(format!("project_{i}"));
            let sub = nested.join("src/deep");
            fs::create_dir_all(&sub).unwrap();
            // A worktree or submodule has a `.git` file, not a directory.
            fs::write(nested.join(marker), "").unwrap();
            assert_eq!(project_root(&sub), Some(nested.clone()), "{marker}");
            assert_eq!(project_root(&nested), Some(nested), "{marker}");
        }

        let python = root.join("venv_project");
        fs::create_dir_all(python.join(".venv/bin")).unwrap();
        fs::create_dir_all(python.join("src/deep")).unwrap();
        fs::write(python.join(".venv/pyvenv.cfg"), "").unwrap();
        fs::write(python.join(".venv/bin/python"), "").unwrap();
        assert_eq!(project_root(&python.join("src/deep")), Some(python));
        fs::remove_dir_all(root).unwrap();
    }

    /// A reflog line for `msg` at `time`.
    fn log_line(time: i64, msg: &str) -> String {
        format!(
            "{0} {0} Me <me@example.com> {time} +0000\t{msg}\n",
            "0".repeat(40)
        )
    }

    #[test]
    fn reflog_lines_give_their_time_and_message() {
        let line = log_line(1_700_000_000, "commit: Fix it");
        assert_eq!(
            reflog_entry(line.trim_end()),
            Some((1_700_000_000, "commit: Fix it"))
        );
        assert_eq!(reflog_entry("junk"), None);
    }

    #[test]
    fn branches_are_read_from_the_repo_files() {
        let root = std::env::temp_dir().join("peregrine_project_branches");
        let _ = fs::remove_dir_all(&root);
        let git = root.join(".git");
        let heads = git.join("refs").join("heads");
        fs::create_dir_all(heads.join("feature")).unwrap();
        fs::create_dir_all(git.join("refs").join("remotes").join("origin")).unwrap();
        fs::create_dir_all(root.join("sub")).unwrap();
        for b in ["main", "old", "feature/x"] {
            fs::write(heads.join(b), "").unwrap();
        }
        fs::write(git.join("refs/remotes/origin/HEAD"), "").unwrap();
        fs::write(
            git.join("packed-refs"),
            "# pack-refs with: peeled\nabc refs/heads/packed\nabc refs/remotes/origin/remote-only\n^def\n",
        )
        .unwrap();

        let now = chrono::Utc::now().timestamp();
        let long_ago = now - 30 * 24 * 60 * 60;
        let logs = git.join("logs").join("refs").join("heads");
        fs::create_dir_all(logs.join("feature")).unwrap();
        fs::write(logs.join("main"), log_line(now - 100, "commit: a")).unwrap();
        fs::write(logs.join("old"), log_line(long_ago, "commit: b")).unwrap();
        fs::write(
            logs.join("feature/x"),
            log_line(long_ago, "branch: Created"),
        )
        .unwrap();
        // Checked out since, though not committed to.
        let head = log_line(long_ago, "checkout: moving from main to old")
            + &log_line(now - 50, "checkout: moving from main to feature/x");
        fs::write(git.join("logs").join("HEAD"), head).unwrap();

        let branches = Branches::read(&root.join("sub"));
        assert_eq!(
            branches.names,
            ["feature/x", "main", "old", "packed", "remote-only"]
        );
        assert_eq!(branches.recent, ["feature/x", "main"]);

        assert!(
            Branches::read(&std::env::temp_dir().join("no_such_repo_dir"))
                .names
                .is_empty()
        );
    }
}
