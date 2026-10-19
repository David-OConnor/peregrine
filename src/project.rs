//! The kind of project the active tab's dir holds, and the command lines the
//! project bar's buttons run (see `ui::project_bar`). Git is
//! told by the tab's branch instead (see `State::branch`), as for the prompt.

use std::{
    env, fs,
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
