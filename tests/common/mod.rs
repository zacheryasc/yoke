//! Shared harness for yoke integration tests: isolated temp project, mock
//! `omp` on PATH, and terse assertions.
#![allow(dead_code, unused_macros, unused_imports)]

use std::fs;
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

unsafe extern "C" {
    fn dup2(oldfd: i32, newfd: i32) -> i32;
    fn fcntl(fd: i32, cmd: i32, arg: i32) -> i32;
}

pub(crate) fn yoke() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_yoke"))
}

pub(crate) fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

pub(crate) fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// A throwaway project directory with an isolated HOME.
pub(crate) struct Project {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    dir: PathBuf,
    home: PathBuf,
}

impl Project {
    pub(crate) fn new() -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().to_path_buf();
        let dir = root.join("project");
        let home = root.join("home");
        fs::create_dir_all(&dir).expect("project");
        fs::create_dir_all(&home).expect("home");
        Self {
            _tmp: tmp,
            root,
            dir,
            home,
        }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.dir
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    pub(crate) fn home(&self) -> &Path {
        &self.home
    }

    pub(crate) fn cmd(&self, args: &[&str], env: &[(&str, &str)]) -> Command {
        let mut c = Command::new(yoke());
        c.args(args).current_dir(&self.dir).env("HOME", &self.home);
        for (k, v) in env {
            c.env(k, v);
        }
        c
    }

    pub(crate) fn run(&self, args: &[&str]) -> Output {
        self.run_env(args, &[])
    }

    pub(crate) fn run_env(&self, args: &[&str], env: &[(&str, &str)]) -> Output {
        self.cmd(args, env)
            .output()
            .unwrap_or_else(|e| panic!("yoke {args:?}: {e}"))
    }

    pub(crate) fn spawn(&self, args: &[&str], env: &[(&str, &str)]) -> Child {
        self.cmd(args, env)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap_or_else(|e| panic!("spawn yoke {args:?}: {e}"))
    }

    pub(crate) fn path_of(&self, rel: &str) -> PathBuf {
        self.dir.join(rel)
    }

    pub(crate) fn write(&self, rel: &str, content: &str) {
        let path = self.path_of(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create parent");
        }
        fs::write(path, content).unwrap_or_else(|e| panic!("write {rel}: {e}"));
    }

    pub(crate) fn read(&self, rel: &str) -> String {
        fs::read_to_string(self.path_of(rel)).unwrap_or_else(|e| panic!("read {rel}: {e}"))
    }

    pub(crate) fn exists(&self, rel: &str) -> bool {
        self.path_of(rel).exists()
    }
}

macro_rules! ok {
    ($out:expr, $what:expr $(,)?) => {
        assert!(
            $out.status.success(),
            "{} failed (exit {:?}):\n{}",
            $what,
            $out.status.code(),
            stderr($out)
        )
    };
}

macro_rules! failed {
    ($out:expr, $what:expr $(,)?) => {
        assert!(
            !$out.status.success(),
            "{} should fail:\n{}",
            $what,
            stderr($out)
        )
    };
}

macro_rules! has {
    ($hay:expr, $needle:expr, $what:expr $(,)?) => {{
        let hay = $hay;
        let needle = $needle;
        assert!(
            hay.contains(needle),
            "{}: missing {:?} in:\n{}",
            $what,
            needle,
            hay
        )
    }};
}

macro_rules! lacks {
    ($hay:expr, $needle:expr, $what:expr $(,)?) => {{
        let hay = $hay;
        let needle = $needle;
        assert!(
            !hay.contains(needle),
            "{}: unexpected {:?} in:\n{}",
            $what,
            needle,
            hay
        )
    }};
}

pub(crate) use {failed, has, lacks, ok};

pub(crate) fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "test")
        .env("GIT_AUTHOR_EMAIL", "test@test")
        .env("GIT_COMMITTER_NAME", "test")
        .env("GIT_COMMITTER_EMAIL", "test@test")
        .output()
        .unwrap_or_else(|e| panic!("git {args:?}: {e}"));
    assert!(
        out.status.success(),
        "git {args:?} failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Initialize a git repo with one seed commit (`.loop/` ignored).
pub(crate) fn git_init(dir: &Path) {
    git(dir, &["init"]);
    fs::write(dir.join("seed.txt"), "seed\n").unwrap();
    fs::write(dir.join(".gitignore"), ".loop/\n").unwrap();
    git(dir, &["add", "seed.txt", ".gitignore"]);
    git(dir, &["commit", "-m", "seed"]);
}

/// Create `.loop/` with the minimal files a run needs, plus a seed commit.
pub(crate) fn seed_loop(p: &Project, conf: &str) {
    git_init(p.path());
    p.write(
        ".loop/protocol.md",
        "# Protocol\nRead plan.md and follow it.\n",
    );
    p.write(".loop/plan.md", "## Stage 1 — Do it\nDo the thing.\n");
    p.write(
        ".loop/judge.md",
        "# Judge\nVerify the change.\n\n## Verdict\nWrite VERDICT.\n",
    );
    p.write(".loop/yoke.conf", conf);
    for f in ["notes.md", "verdict.md", "guard-results.md"] {
        p.write(&format!(".loop/{f}"), "");
    }
}

/// Install a named executable into the out-of-repo mock bin dir.
pub(crate) fn install_mock_exe(p: &Project, name: &str, script: &str) {
    let path = p.root().join("mock-bin").join(name);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, script).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
}

/// Install `script` as a mock `omp` and return a PATH string that finds it first.
pub(crate) fn install_mock(p: &Project, script: &str) -> String {
    install_mock_exe(p, "omp", script);
    format!(
        "{}:{}",
        p.root().join("mock-bin").display(),
        std::env::var("PATH").unwrap_or_default()
    )
}

pub(crate) fn wait_until(timeout: Duration, mut predicate: impl FnMut() -> bool) {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if predicate() {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("condition not met within {timeout:?}");
}

pub(crate) fn wait_logged(timeout: Duration, log: &Path, needle: &str) {
    wait_until(timeout, || {
        fs::read_to_string(log).unwrap_or_default().contains(needle)
    });
}

pub(crate) fn wait_output(mut child: Child, timeout: Duration) -> Output {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if child.try_wait().expect("try_wait").is_some() {
            return child.wait_with_output().expect("wait_with_output");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = child.kill();
    let out = child.wait_with_output().expect("output after kill");
    panic!(
        "process timed out\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Spawn `yoke run` with summary records wired to fd 3.
pub(crate) fn spawn_with_summary(
    p: &Project,
    env: &[(&str, &str)],
    stderr: Stdio,
) -> (Child, UnixStream) {
    let (child_end, parent_end) = UnixStream::pair().expect("socket pair");
    let mut cmd = p.cmd(&["run", "--no-sandbox", "--summary-fd", "3"], env);
    cmd.stdout(Stdio::null()).stderr(stderr);
    let fd = child_end.as_raw_fd();
    unsafe {
        cmd.pre_exec(move || {
            if dup2(fd, 3) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            // UnixStream fds are close-on-exec; dup2(3, 3) does not clear it.
            if fcntl(3, 2, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = cmd.spawn().expect("spawn yoke run");
    drop(child_end);
    (child, parent_end)
}

pub(crate) fn summary_records(stream: UnixStream) -> Vec<serde_json::Value> {
    use std::io::BufRead;
    std::io::BufReader::new(stream)
        .lines()
        .map_while(Result::ok)
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(&line).expect("summary record JSON"))
        .collect()
}

/// One mock `omp` for every scenario. Behavior is selected by env vars set on
/// the yoke invocation: `MOCK_JUDGE` (pass|fail|fail-first|fail-always),
/// `MOCK_WORKER` (done|cost|patch|await-steer|pause|summary|slow-summary),
/// `MOCK_STEER` (accept|reject), `MOCK_SUMMARY` (cheap|slow), `MOCK_WITNESS`.
pub(crate) const MOCK: &str = include_str!("omp_mock.sh");
