//! The repos in the remote workspace a repo was cloned from: everything under
//! `github.com/david-oconnor` for `github.com/david-oconnor/molchanica`, say.
//! Listed a page at a time from GitHub's or GitLab's API, for the project
//! bar's Projects window (see `ui::projects`).

use std::{env, path::Path, time::Duration};

use serde::{Deserialize, de::IgnoredAny};
use shell::quiet_command;
use ureq::{Agent, Body, http::Response};

/// Repos to a page.
const PER_PAGE: u32 = 50;

/// Which API a host has.
#[derive(Clone, Copy, PartialEq)]
enum Forge {
    GitHub,
    GitLab,
}

/// Where a repo's remote is, from its URL.
pub struct Workspace {
    forge: Forge,
    /// With the port, for an HTTP remote.
    host: String,
    /// `http` for a remote cloned over it; else `https`.
    scheme: &'static str,
    /// The path under the host, short of the repo: a user or org, or a GitLab
    /// group and its subgroups (`group/sub`).
    namespace: String,
    /// Cloned over SSH, so the listed repos are too.
    ssh: bool,
}

impl Workspace {
    /// From a remote's URL: `https://host/ns/repo`, `ssh://[user@]host/ns/repo`,
    /// or scp-style `[user@]host:ns/repo`. `None` for a local path, or a repo
    /// with no namespace.
    fn parse(url: &str) -> Option<Self> {
        let url = url.trim().trim_end_matches('/');
        let (scheme, authority, path) = match url.split_once("://") {
            Some((scheme, rest)) => {
                let (authority, path) = rest.split_once('/')?;
                (scheme, authority, path)
            }
            None => {
                // As git tells it from a local path.
                let (authority, path) = url.split_once(':')?;
                if authority.len() < 2 || authority.contains(['/', '\\']) {
                    return None;
                }
                ("ssh", authority, path)
            }
        };

        let ssh = match scheme {
            "http" | "https" | "git" => false,
            "ssh" | "git+ssh" | "ssh+git" => true,
            _ => return None,
        };
        let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
        // An SSH port isn't the web server's.
        let host = if ssh { host.split(':').next()? } else { host };
        // GitHub's SSH over port 443.
        let host = if host == "ssh.github.com" {
            "github.com"
        } else {
            host
        };

        let path = path.strip_suffix(".git").unwrap_or(path);
        let (namespace, repo) = path.trim_start_matches('/').rsplit_once('/')?;
        if host.is_empty() || namespace.is_empty() || repo.is_empty() {
            return None;
        }

        Some(Self {
            forge: if host.contains("github") {
                Forge::GitHub
            } else {
                Forge::GitLab
            },
            host: host.to_owned(),
            scheme: if scheme == "http" { "http" } else { "https" },
            namespace: namespace.to_owned(),
            ssh,
        })
    }

    /// Of the repo at `dir`: its `origin` remote's, else its first one's.
    fn of_repo(dir: &Path) -> Result<Self, String> {
        let git = |args: &[&str]| {
            quiet_command("git")
                .args(args)
                .current_dir(dir)
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        };

        let remotes = git(&["remote"]).unwrap_or_default();
        let remote = remotes
            .lines()
            .find(|r| *r == "origin")
            .or_else(|| remotes.lines().next())
            .ok_or("This repo has no remote.")?;
        let url = git(&["remote", "get-url", remote])
            .ok_or_else(|| format!("Unable to read remote `{remote}`'s URL."))?;

        Self::parse(&url).ok_or_else(|| format!("Unable to tell the workspace of `{url}`."))
    }

    /// E.g. `github.com/david-oconnor`.
    pub fn label(&self) -> String {
        format!("{}/{}", self.host, self.namespace)
    }

    pub fn web_url(&self) -> String {
        format!("{}://{}/{}", self.scheme, self.host, self.namespace)
    }

    fn api(&self) -> String {
        match self.forge {
            Forge::GitHub if self.host == "github.com" => "https://api.github.com".to_owned(),
            Forge::GitHub => format!("{}://{}/api/v3", self.scheme, self.host),
            Forge::GitLab => format!("{}://{}/api/v4", self.scheme, self.host),
        }
    }

    /// The variables a token is read from, as GitHub's and GitLab's CLIs
    /// read them.
    fn token_vars(&self) -> &'static [&'static str] {
        match self.forge {
            Forge::GitHub if self.host == "github.com" => &["GH_TOKEN", "GITHUB_TOKEN"],
            Forge::GitHub => &["GH_ENTERPRISE_TOKEN", "GITHUB_ENTERPRISE_TOKEN"],
            Forge::GitLab => &["GITLAB_TOKEN"],
        }
    }

    /// A token to list private repos with: from [Self::token_vars], or for
    /// GitHub, a signed-in `gh`'s.
    fn token(&self) -> Option<String> {
        let from_env = self
            .token_vars()
            .iter()
            .filter_map(|v| env::var(v).ok())
            .find(|t| !t.trim().is_empty());
        if from_env.is_some() || self.forge != Forge::GitHub {
            return from_env;
        }

        let out = quiet_command("gh")
            .args(["auth", "token", "--hostname", &self.host])
            .output()
            .ok()
            .filter(|o| o.status.success())?;
        let token = String::from_utf8_lossy(&out.stdout).trim().to_owned();

        (!token.is_empty()).then_some(token)
    }
}

/// A repo in a workspace.
pub struct RemoteRepo {
    /// The dir `git clone` makes for it.
    pub name: String,
    pub description: String,
    /// Over SSH or HTTPS, as the workspace's repo was cloned.
    pub clone_url: String,
    pub web_url: String,
    /// `private` or `internal`; empty for a public repo.
    pub visibility: String,
    pub fork: bool,
    pub archived: bool,
}

/// The parts of GitHub's repo JSON used.
#[derive(Deserialize)]
struct GithubRepo {
    name: String,
    description: Option<String>,
    clone_url: String,
    ssh_url: String,
    html_url: String,
    #[serde(default)]
    visibility: String,
    #[serde(default)]
    fork: bool,
    #[serde(default)]
    archived: bool,
}

/// The parts of GitLab's project JSON used.
#[derive(Deserialize)]
struct GitlabProject {
    path: String,
    description: Option<String>,
    http_url_to_repo: String,
    ssh_url_to_repo: String,
    web_url: String,
    #[serde(default)]
    visibility: String,
    #[serde(default)]
    forked_from_project: Option<IgnoredAny>,
    #[serde(default)]
    archived: bool,
}

/// One page of a workspace's repos, by name.
pub struct Page {
    pub repos: Vec<RemoteRepo>,
    /// From 1.
    pub number: u32,
    /// The last page's number, where the API says.
    pub last: Option<u32>,
    pub has_next: bool,
}

/// Where a workspace's repos are listed.
pub struct Listing {
    pub workspace: Workspace,
    token: Option<String>,
    /// The API URL listing them, short of the page.
    url: String,
    agent: Agent,
}

impl Listing {
    /// For the repo at `dir`. Asks the API whether the workspace is a user,
    /// org, or group, so run it off the UI thread.
    pub fn find(dir: &Path) -> Result<Self, String> {
        let workspace = Workspace::of_repo(dir)?;
        let agent = Agent::config_builder()
            // GitHub's API rejects requests without one.
            .user_agent(concat!("peregrine/", env!("CARGO_PKG_VERSION")))
            .timeout_global(Some(Duration::from_secs(30)))
            .build()
            .new_agent();

        let mut listing = Self {
            token: workspace.token(),
            workspace,
            url: String::new(),
            agent,
        };
        listing.url = match listing.workspace.forge {
            Forge::GitHub => listing.github_url(),
            Forge::GitLab => listing.gitlab_url()?,
        };

        Ok(listing)
    }

    /// Signed in, a user's own repos and an org's include private ones.
    /// `/users/<name>/repos` lists a user's or an org's public ones. Drops a
    /// token GitHub refuses.
    fn github_url(&mut self) -> String {
        #[derive(Deserialize)]
        struct User {
            login: String,
        }

        let api = self.workspace.api();
        let owner = encode(&self.workspace.namespace);
        let public = format!("{api}/users/{owner}/repos?sort=full_name");
        if self.token.is_none() {
            return public;
        }

        let me = self
            .get(&format!("{api}/user"))
            .ok()
            .and_then(|mut r| r.body_mut().read_json::<User>().ok());
        let Some(me) = me else {
            self.token = None;
            return public;
        };

        if me.login.eq_ignore_ascii_case(&self.workspace.namespace) {
            format!("{api}/user/repos?affiliation=owner&sort=full_name")
        } else if self.get(&format!("{api}/orgs/{owner}")).is_ok() {
            format!("{api}/orgs/{owner}/repos?type=all&sort=full_name")
        } else {
            public
        }
    }

    /// A group's projects, or failing that, a user's.
    fn gitlab_url(&self) -> Result<String, String> {
        let api = self.workspace.api();
        let ns = encode(&self.workspace.namespace);
        let order = "order_by=path&sort=asc";
        let group = format!("{api}/groups/{ns}/projects?{order}");

        match self.get(&format!("{group}&per_page=1")) {
            Ok(_) => Ok(group),
            Err(ureq::Error::StatusCode(404)) => Ok(format!("{api}/users/{ns}/projects?{order}")),
            Err(e) => Err(self.describe(e)),
        }
    }

    /// Page `n`, from 1. Makes a request, so run it off the UI thread.
    pub fn page(&self, n: u32) -> Result<Page, String> {
        let url = format!("{}&per_page={PER_PAGE}&page={n}", self.url);
        let mut resp = self.get(&url).map_err(|e| self.describe(e))?;

        let header = |name| {
            resp.headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
                .to_owned()
        };
        let (has_next, last) = match self.workspace.forge {
            Forge::GitHub => {
                let link = header("link");
                (link_page(&link, "next").is_some(), link_page(&link, "last"))
            }
            Forge::GitLab => (
                !header("x-next-page").is_empty(),
                header("x-total-pages").parse().ok(),
            ),
        };

        let ssh = self.workspace.ssh;
        let body = resp.body_mut();
        let bad_json = |e: ureq::Error| format!("Unable to read the list: {e}");
        let repos = match self.workspace.forge {
            Forge::GitHub => body
                .read_json::<Vec<GithubRepo>>()
                .map_err(bad_json)?
                .into_iter()
                .map(|r| RemoteRepo {
                    name: r.name,
                    description: r.description.unwrap_or_default(),
                    clone_url: if ssh { r.ssh_url } else { r.clone_url },
                    web_url: r.html_url,
                    visibility: public_blank(r.visibility),
                    fork: r.fork,
                    archived: r.archived,
                })
                .collect(),
            Forge::GitLab => body
                .read_json::<Vec<GitlabProject>>()
                .map_err(bad_json)?
                .into_iter()
                .map(|p| RemoteRepo {
                    name: p.path,
                    description: p.description.unwrap_or_default(),
                    clone_url: if ssh {
                        p.ssh_url_to_repo
                    } else {
                        p.http_url_to_repo
                    },
                    web_url: p.web_url,
                    visibility: public_blank(p.visibility),
                    fork: p.forked_from_project.is_some(),
                    archived: p.archived,
                })
                .collect(),
        };

        Ok(Page {
            repos,
            number: n,
            // The last page has no link to itself.
            last: last.or((!has_next).then_some(n)),
            has_next,
        })
    }

    fn get(&self, url: &str) -> Result<Response<Body>, ureq::Error> {
        let mut req = self.agent.get(url);
        if self.workspace.forge == Forge::GitHub {
            req = req.header("Accept", "application/vnd.github+json");
        }
        // GitLab takes personal access tokens this way too.
        if let Some(t) = &self.token {
            req = req.header("Authorization", format!("Bearer {t}"));
        }

        req.call()
    }

    /// `e`, for the window.
    fn describe(&self, e: ureq::Error) -> String {
        let host = &self.workspace.host;
        let vars = self.workspace.token_vars().join(" or ");
        match e {
            ureq::Error::StatusCode(401 | 403 | 429) if self.token.is_none() => format!(
                "{host} refused: it needs a sign-in, or its rate limit was reached. \
                 Set {vars} to an access token."
            ),
            ureq::Error::StatusCode(401 | 403 | 429) => {
                format!("{host} refused the token, or its rate limit was reached.")
            }
            ureq::Error::StatusCode(404) => {
                format!("{host} has no `{}` visible.", self.workspace.namespace)
            }
            e => format!("Unable to reach {host}: {e}"),
        }
    }
}

/// `visibility`, empty when `public`.
fn public_blank(visibility: String) -> String {
    if visibility == "public" {
        String::new()
    } else {
        visibility
    }
}

/// The page number of the `rel` link in GitHub's `Link` header:
/// `<https://…&page=2>; rel="next", <https://…&page=5>; rel="last"`.
fn link_page(link: &str, rel: &str) -> Option<u32> {
    let rel = format!("rel=\"{rel}\"");
    let part = link.split(',').find(|p| p.contains(&rel))?;
    let url = part.split(['<', '>']).nth(1)?;
    let (_, query) = url.split_once('?')?;

    query
        .split('&')
        .find_map(|kv| kv.strip_prefix("page="))?
        .parse()
        .ok()
}

/// `s` percent-encoded as one URL path segment, as GitLab takes a group's
/// full path.
fn encode(s: &str) -> String {
    s.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}
