//! Per-team workspace isolation.
//!
//! A team is the unit. Every agent in a team shares one workspace; agents in
//! different teams never share one. Three modes:
//!
//! - `none`: what OMAR did before — every agent launches in the configured
//!   workdir, teams share it.
//! - `worktree`: a git worktree per deployment, cut from the configured
//!   workdir's repository onto a branch named for the run.
//! - `container`: the worktree, plus a docker container per deployment that
//!   mounts it. Agent panes run `docker exec` into that container.
//!
//! Worktree isolation separates working trees, not repositories: the object
//! database is shared, so a team can read another team's branches. It buys
//! coordination, not security. Container isolation adds an OS boundary around
//! the team, but OMAR's tmux server stays outside it, so an agent that can
//! reach the MCP surface can still ask for a pane on the host.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};

use crate::config::IsolationConfig;
use crate::manager::shell_single_quote;

/// Which boundary a deployment runs behind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    None,
    Worktree,
    Container,
}

impl Mode {
    pub fn parse(raw: &str) -> Result<Mode> {
        match raw.trim() {
            "none" => Ok(Mode::None),
            "worktree" => Ok(Mode::Worktree),
            "container" => Ok(Mode::Container),
            other => bail!(
                "unknown isolation mode '{}'. Supported: none, worktree, container",
                other
            ),
        }
    }
}

/// A prepared workspace, owned by one deployment.
///
/// `workdir` is a host path in every mode. Container mode mounts the worktree
/// at the same absolute path inside the container, so a path that OMAR baked
/// into a launch command — the MCP config, the system prompt, the omar binary
/// — resolves identically on both sides and needs no translation.
#[derive(Debug, Clone)]
pub struct Workspace {
    workdir: String,
    worktree: Option<PathBuf>,
    container: Option<String>,
}

impl Workspace {
    /// Where agents launch. Absolute in every mode but `none`, which passes
    /// the operator's configured value through untouched.
    pub fn workdir(&self) -> &str {
        &self.workdir
    }

    /// Wrap an agent launch command for this workspace.
    ///
    /// Only container mode rewrites anything. The command is handed to the
    /// container's shell as a single quoted argument, so the quoting a backend
    /// command already carries — codex's `-c key='"value"'` — survives.
    pub fn wrap_command(&self, command: &str) -> String {
        match &self.container {
            Some(name) => format!(
                "docker exec -it -w {} {} sh -lc {}",
                shell_single_quote(&self.workdir),
                shell_single_quote(name),
                shell_single_quote(command)
            ),
            None => command.to_string(),
        }
    }

    /// Release the workspace.
    ///
    /// `keep` holds the worktree on disk for a run that failed or was
    /// cancelled, which is exactly when an operator needs to look at what the
    /// team did. The container goes either way: it is reconstructible from the
    /// worktree, and a stopped run should not leave one running.
    pub fn teardown(&self, keep: bool) -> Vec<String> {
        let mut failures = Vec::new();
        if let Some(name) = &self.container {
            if let Err(error) = docker(&["rm", "-f", name]) {
                failures.push(format!("container {name}: {error:#}"));
            }
        }
        if keep {
            return failures;
        }
        if let Some(path) = &self.worktree {
            if let Err(error) = remove_worktree(path) {
                failures.push(format!("worktree {}: {error:#}", path.display()));
            }
        }
        failures
    }
}

/// Build the workspace a deployment runs in.
///
/// `source` is the operator's configured workdir — the repository a worktree
/// is cut from. `runtime_dir` is the team's own state directory, which already
/// holds the deployment record and logs, so the worktree lands beside them and
/// `omar stop` needs no new path to know about.
pub fn prepare(
    settings: &IsolationConfig,
    ea_id: crate::ea::EaId,
    team: &str,
    deployment_id: &str,
    source: &str,
    runtime_dir: &Path,
    backends: &[String],
) -> Result<Workspace> {
    let mode = Mode::parse(&settings.mode)?;
    if mode == Mode::None {
        return Ok(Workspace {
            workdir: source.to_string(),
            worktree: None,
            container: None,
        });
    }

    let path = runtime_dir.join("worktree");
    let branch = branch_name(team, deployment_id);
    create_worktree(source, &path, &branch)?;
    let workdir = path.to_string_lossy().into_owned();

    if mode == Mode::Worktree {
        return Ok(Workspace {
            workdir,
            worktree: Some(path),
            container: None,
        });
    }

    let name = container_name(ea_id, team);
    match start_container(settings, &name, &workdir, backends) {
        Ok(()) => Ok(Workspace {
            workdir,
            worktree: Some(path),
            container: Some(name),
        }),
        Err(error) => {
            // The worktree exists but the run will not start. Leaving it would
            // strand a branch nobody asked for.
            let _ = remove_worktree(&path);
            Err(error)
        }
    }
}

/// The branch a deployment's worktree checks out.
///
/// Namespaced by team and run so a second deployment of the same team never
/// collides with a branch the first one still holds — git refuses to check out
/// one branch in two worktrees, and that refusal would surface as a confusing
/// deploy failure rather than the isolation it actually is.
pub fn branch_name(team: &str, deployment_id: &str) -> String {
    format!("omar/{}/{}", sanitize(team), short_id(deployment_id))
}

/// The container a team runs in.
///
/// Scoped to the team rather than the run, unlike the branch: OMAR already
/// refuses a second live run of one team, and a stable name means a container
/// orphaned by a runner that died is reclaimed by the next deploy instead of
/// leaking. The EA id is in it because two EAs may each have a team `alpha`.
pub fn container_name(ea_id: crate::ea::EaId, team: &str) -> String {
    format!("omar-{}-{}", ea_id, sanitize(team))
}

/// Keep only characters both git refs and docker names accept, so one
/// sanitizer serves both and a team name can't inject an argument.
fn sanitize(raw: &str) -> String {
    let cleaned: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let trimmed = cleaned.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "team".to_string()
    } else {
        trimmed
    }
}

/// Deployment ids are uuids; the first segment is enough to separate runs and
/// keeps the branch and container names readable.
fn short_id(deployment_id: &str) -> String {
    let head: String = deployment_id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(8)
        .collect();
    if head.is_empty() {
        "run".to_string()
    } else {
        head
    }
}

fn create_worktree(source: &str, path: &Path, branch: &str) -> Result<()> {
    let repo = std::fs::canonicalize(source)
        .with_context(|| format!("isolation source directory '{source}' does not exist"))?;
    if !is_git_repo(&repo) {
        bail!(
            "isolation needs a git repository: '{}' is not inside one. \
             Set [agent] default_workdir to a repository, or use isolation mode 'none'.",
            repo.display()
        );
    }
    // A previous run of this team may have left one behind — a failed run
    // keeps its worktree on purpose. Clear it before reusing the path.
    if path.exists() {
        remove_worktree(path)?;
    }
    // Registrations can outlive their directory (a manual `rm -rf`), and git
    // refuses to add a worktree whose path it still has on file.
    let _ = git(&repo, &["worktree", "prune"]);
    // `-B` rather than `-b`: a branch left over from a run whose worktree was
    // already removed must not block the new one.
    git(
        &repo,
        &[
            "worktree",
            "add",
            "-B",
            branch,
            &path.to_string_lossy(),
            "HEAD",
        ],
    )
    .with_context(|| format!("create worktree for branch '{branch}'"))?;
    Ok(())
}

fn remove_worktree(path: &Path) -> Result<()> {
    // Resolve the repository from the worktree itself: the caller's source
    // directory is not reachable from teardown, and a worktree always knows
    // its own common directory.
    let repo = match git_common_dir(path) {
        Some(dir) => dir,
        None => {
            // Not a live worktree any more. Nothing to unregister.
            if path.exists() {
                std::fs::remove_dir_all(path).ok();
            }
            return Ok(());
        }
    };
    git(
        &repo,
        &["worktree", "remove", "--force", &path.to_string_lossy()],
    )
    .with_context(|| format!("remove worktree '{}'", path.display()))?;
    Ok(())
}

fn is_git_repo(dir: &Path) -> bool {
    git(dir, &["rev-parse", "--git-dir"]).is_ok()
}

/// The main repository a worktree belongs to, or `None` if the path is not a
/// worktree at all.
fn git_common_dir(worktree: &Path) -> Option<PathBuf> {
    if !worktree.exists() {
        return None;
    }
    let common = git(
        worktree,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )
    .ok()?;
    let common = PathBuf::from(common.trim());
    // `.../repo/.git` -> `.../repo`
    common.parent().map(Path::to_path_buf)
}

fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .context("run git")?;
    if !output.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Start the team's container and prove the backends it must run are present.
fn start_container(
    settings: &IsolationConfig,
    name: &str,
    workdir: &str,
    backends: &[String],
) -> Result<()> {
    if Command::new("docker")
        .arg("--version")
        .output()
        .map(|out| !out.status.success())
        .unwrap_or(true)
    {
        bail!("isolation mode 'container' needs docker on PATH");
    }
    // A container left by a run that died before teardown holds the name.
    let _ = docker(&["rm", "-f", name]);

    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));
    let home = home.to_string_lossy().into_owned();
    let mut args: Vec<String> = vec![
        "run".into(),
        "-d".into(),
        "--name".into(),
        name.into(),
        // Host paths are mounted at the same absolute paths inside, and HOME
        // points at the host home, so `~/.claude` and the absolute paths baked
        // into a launch command mean the same thing on both sides.
        "-e".into(),
        format!("HOME={home}"),
        "-w".into(),
        workdir.into(),
        "-v".into(),
        format!("{workdir}:{workdir}"),
    ];
    for mount in mount_paths(settings, &home) {
        args.push("-v".into());
        args.push(format!("{mount}:{mount}"));
    }
    args.push(settings.image.clone());
    // Keep the container alive with no agent in it: panes join later with
    // `docker exec`, and each pane's lifetime is its own.
    args.extend(["sleep".to_string(), "infinity".to_string()]);

    let borrowed: Vec<&str> = args.iter().map(String::as_str).collect();
    docker(&borrowed)
        .with_context(|| format!("start container from image '{}'", settings.image))?;

    if let Err(error) = verify_backends(name, backends, &settings.image) {
        let _ = docker(&["rm", "-f", name]);
        return Err(error);
    }
    Ok(())
}

/// Every host path the container needs mounted, deduplicated and existing.
///
/// A mount of a path that does not exist makes docker create an empty
/// directory owned by root on the host, so a typo in `credential_mounts`
/// would litter the operator's home rather than fail.
fn mount_paths(settings: &IsolationConfig, home: &str) -> Vec<String> {
    let mut paths: Vec<String> = Vec::new();
    // OMAR's own state: the MCP config, the system prompt and the context file
    // a pane's sidecar reads are all under it, by absolute path.
    if let Some(dir) = dirs::home_dir() {
        paths.push(dir.join(".omar").to_string_lossy().into_owned());
    }
    // The MCP server a backend launches is this binary.
    if let Ok(exe) = std::env::current_exe() {
        paths.push(exe.to_string_lossy().into_owned());
    }
    for raw in &settings.credential_mounts {
        paths.push(expand_home(raw, home));
    }
    let mut seen = std::collections::BTreeSet::new();
    paths
        .into_iter()
        .filter(|path| Path::new(path).exists())
        .filter(|path| seen.insert(path.clone()))
        .collect()
}

fn expand_home(raw: &str, home: &str) -> String {
    match raw.strip_prefix("~/") {
        Some(rest) => format!("{home}/{rest}"),
        None => raw.to_string(),
    }
}

/// Fail a container deploy that would leave every pane at "command not found".
///
/// The default image carries no agent CLIs, so this is the common case rather
/// than a defensive one, and the message has to say what to do about it.
fn verify_backends(container: &str, backends: &[String], image: &str) -> Result<()> {
    let mut missing: Vec<&str> = Vec::new();
    for backend in backends {
        let probe = format!("command -v {}", shell_single_quote(backend));
        if docker(&["exec", container, "sh", "-lc", &probe]).is_err() {
            missing.push(backend);
        }
    }
    if missing.is_empty() {
        return Ok(());
    }
    bail!(
        "image '{}' has no {} on PATH. Container isolation runs each agent inside \
         the image, so it must carry every backend the team uses. Set [isolation] image \
         to one that does — see docs/isolation.md for an example Dockerfile.",
        image,
        missing.join(", ")
    )
}

fn docker(args: &[&str]) -> Result<String> {
    let output = Command::new("docker")
        .args(args)
        .output()
        .context("run docker")?;
    if !output.status.success() {
        bail!(
            "docker {} failed: {}",
            args.first().copied().unwrap_or_default(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(mode: &str) -> IsolationConfig {
        IsolationConfig {
            mode: mode.to_string(),
            image: "debian:bookworm-slim".to_string(),
            credential_mounts: Vec::new(),
        }
    }

    #[test]
    fn mode_parses_the_three_supported_values() {
        assert_eq!(Mode::parse("none").unwrap(), Mode::None);
        assert_eq!(Mode::parse("worktree").unwrap(), Mode::Worktree);
        assert_eq!(Mode::parse("container").unwrap(), Mode::Container);
        assert!(Mode::parse("vm").is_err());
    }

    #[test]
    fn none_passes_the_configured_workdir_through_untouched() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = prepare(
            &settings("none"),
            0,
            "alpha",
            "1234abcd-0000",
            ".",
            temp.path(),
            &["claude".to_string()],
        )
        .unwrap();
        assert_eq!(workspace.workdir(), ".");
        // And nothing wraps the launch: the pane runs on the host as before.
        assert_eq!(workspace.wrap_command("claude"), "claude");
    }

    #[test]
    fn a_command_is_unchanged_without_a_container() {
        let workspace = Workspace {
            workdir: "/w".to_string(),
            worktree: None,
            container: None,
        };
        assert_eq!(workspace.wrap_command("claude --foo"), "claude --foo");
    }

    #[test]
    fn a_container_command_survives_the_quoting_a_backend_already_carries() {
        let workspace = Workspace {
            workdir: "/w/one".to_string(),
            worktree: None,
            container: Some("omar-alpha-1234abcd".to_string()),
        };
        let wrapped =
            workspace.wrap_command("codex --no-alt-screen -c model_reasoning_effort='\"high\"'");
        assert_eq!(
            wrapped,
            "docker exec -it -w '/w/one' 'omar-alpha-1234abcd' sh -lc \
             'codex --no-alt-screen -c model_reasoning_effort='\\''\"high\"'\\'''"
        );
        // And the wrapper is what a shell would hand back as one argument.
        let echoed = Command::new("sh")
            .args([
                "-lc",
                &format!(
                    "printf '%s' {}",
                    shell_single_quote(
                        "codex --no-alt-screen -c model_reasoning_effort='\"high\"'",
                    )
                ),
            ])
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&echoed.stdout),
            "codex --no-alt-screen -c model_reasoning_effort='\"high\"'"
        );
    }

    #[test]
    fn a_branch_is_namespaced_by_team_and_run() {
        assert_eq!(branch_name("alpha", "1234abcd-ef"), "omar/alpha/1234abcd");
        // Two runs of one team never share a branch, or the second would
        // force-reset the first one's commits away.
        assert_ne!(
            branch_name("alpha", "1234abcd"),
            branch_name("alpha", "9999zzzz")
        );
    }

    #[test]
    fn a_container_is_named_for_the_team_so_a_stale_one_is_reclaimed() {
        assert_eq!(container_name(0, "alpha"), "omar-0-alpha");
        // Stable across runs of the same team...
        assert_eq!(container_name(0, "alpha"), container_name(0, "alpha"));
        // ...and distinct for the same team name under another EA.
        assert_ne!(container_name(0, "alpha"), container_name(1, "alpha"));
    }

    #[test]
    fn a_team_name_cannot_inject_a_git_or_docker_argument() {
        assert_eq!(branch_name("--force ; rm", "abc"), "omar/force---rm/abc");
        assert_eq!(container_name(0, "../../etc"), "omar-0-etc");
        assert_eq!(branch_name("", "abc"), "omar/team/abc");
    }

    #[test]
    fn a_credential_mount_expands_a_leading_tilde() {
        assert_eq!(expand_home("~/.claude", "/home/x"), "/home/x/.claude");
        assert_eq!(expand_home("/etc/ssl", "/home/x"), "/etc/ssl");
    }

    #[test]
    fn mounts_skip_paths_that_do_not_exist() {
        let temp = tempfile::tempdir().unwrap();
        let present = temp.path().join("present");
        std::fs::create_dir(&present).unwrap();
        let mut config = settings("container");
        config.credential_mounts = vec![
            present.to_string_lossy().into_owned(),
            temp.path().join("absent").to_string_lossy().into_owned(),
        ];
        let mounts = mount_paths(&config, "/home/x");
        assert!(mounts.contains(&present.to_string_lossy().into_owned()));
        assert!(!mounts.iter().any(|m| m.ends_with("absent")));
    }

    #[cfg(unix)]
    fn repo_with_a_commit() -> tempfile::TempDir {
        let temp = tempfile::tempdir().unwrap();
        let run = |args: &[&str]| {
            let ok = Command::new("git")
                .current_dir(temp.path())
                .args(args)
                .output()
                .unwrap()
                .status
                .success();
            assert!(ok, "git {args:?} failed");
        };
        run(&["init", "-q"]);
        run(&["config", "user.email", "test@example.com"]);
        run(&["config", "user.name", "test"]);
        std::fs::write(temp.path().join("file.txt"), "one").unwrap();
        run(&["add", "."]);
        run(&["commit", "-qm", "first"]);
        temp
    }

    #[cfg(unix)]
    #[test]
    fn worktree_mode_gives_each_team_its_own_checkout_of_one_repo() {
        let repo = repo_with_a_commit();
        let source = repo.path().to_string_lossy().into_owned();
        let state = tempfile::tempdir().unwrap();
        let alpha_dir = state.path().join("alpha");
        let beta_dir = state.path().join("beta");
        std::fs::create_dir_all(&alpha_dir).unwrap();
        std::fs::create_dir_all(&beta_dir).unwrap();

        let alpha = prepare(
            &settings("worktree"),
            0,
            "alpha",
            "aaaaaaaa",
            &source,
            &alpha_dir,
            &[],
        )
        .unwrap();
        let beta = prepare(
            &settings("worktree"),
            0,
            "beta",
            "bbbbbbbb",
            &source,
            &beta_dir,
            &[],
        )
        .unwrap();

        // Separate directories, both real checkouts of the same commit.
        assert_ne!(alpha.workdir(), beta.workdir());
        assert_eq!(
            std::fs::read_to_string(Path::new(alpha.workdir()).join("file.txt")).unwrap(),
            "one"
        );
        assert_eq!(
            std::fs::read_to_string(Path::new(beta.workdir()).join("file.txt")).unwrap(),
            "one"
        );

        // A write by one team is invisible to the other.
        std::fs::write(Path::new(alpha.workdir()).join("file.txt"), "alpha").unwrap();
        assert_eq!(
            std::fs::read_to_string(Path::new(beta.workdir()).join("file.txt")).unwrap(),
            "one"
        );

        // Teardown unregisters the worktree rather than orphaning it.
        assert!(alpha.teardown(false).is_empty());
        assert!(!Path::new(alpha.workdir()).exists());
        let listed = git(repo.path(), &["worktree", "list"]).unwrap();
        assert!(
            !listed.contains("alpha"),
            "worktree still registered: {listed}"
        );
        assert!(beta.teardown(false).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn a_failed_run_keeps_its_worktree_for_inspection() {
        let repo = repo_with_a_commit();
        let state = tempfile::tempdir().unwrap();
        let workspace = prepare(
            &settings("worktree"),
            0,
            "alpha",
            "aaaaaaaa",
            &repo.path().to_string_lossy(),
            state.path(),
            &[],
        )
        .unwrap();
        std::fs::write(Path::new(workspace.workdir()).join("evidence.txt"), "why").unwrap();

        assert!(workspace.teardown(true).is_empty());

        assert!(Path::new(workspace.workdir()).join("evidence.txt").exists());
        // And the next run of the same team reclaims the path rather than
        // failing on it.
        let next = prepare(
            &settings("worktree"),
            0,
            "alpha",
            "cccccccc",
            &repo.path().to_string_lossy(),
            state.path(),
            &[],
        )
        .unwrap();
        assert!(!Path::new(next.workdir()).join("evidence.txt").exists());
        assert!(next.teardown(false).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn worktree_mode_refuses_a_source_that_is_not_a_repository() {
        let plain = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let error = prepare(
            &settings("worktree"),
            0,
            "alpha",
            "aaaaaaaa",
            &plain.path().to_string_lossy(),
            state.path(),
            &[],
        )
        .unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("git repository"), "{message}");
        assert!(message.contains("isolation mode 'none'"), "{message}");
    }
}
