use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;

fn yoke_bin() -> std::path::PathBuf {
    let mut path = std::env::current_exe()
        .expect("current_exe")
        .parent()
        .expect("parent of test binary")
        .parent()
        .expect("parent of deps dir")
        .to_path_buf();
    path.push("yoke");
    path
}

fn git_init(project: &std::path::Path) {
    let git = |args: &[&str]| {
        let out = Command::new("git")
            .args(args)
            .current_dir(project)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "test")
            .env("GIT_AUTHOR_EMAIL", "test@test")
            .env("GIT_COMMITTER_NAME", "test")
            .env("GIT_COMMITTER_EMAIL", "test@test")
            .output()
            .unwrap_or_else(|e| panic!("git {:?} failed: {}", args, e));
        assert!(
            out.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
    };
    git(&["init"]);
    fs::write(project.join("seed.txt"), "seed\n").unwrap();
    git(&["add", "seed.txt"]);
    git(&[
        "-c",
        "user.name=test",
        "-c",
        "user.email=test@test",
        "commit",
        "-m",
        "init",
    ]);
}

const MOCK_OMP: &str = r#"#!/usr/bin/env bash
set -euo pipefail
{
    echo "CALL"
    for arg in "$@"; do
        echo "$arg"
    done
} >> .loop/.omp-argv
printf 'STATUS: DONE\n' > .loop/notes.md
exit 0
"#;

#[test]
fn default_backend_invokes_omp_with_model_and_thinking() {
    let status = Command::new("cargo")
        .args(["build", "--quiet"])
        .status()
        .expect("cargo build");
    assert!(status.success(), "cargo build failed");

    let yoke = yoke_bin();
    let tmp = tempfile::tempdir().expect("tempdir");
    let project = tmp.path();
    git_init(project);

    let loop_dir = project.join(".loop");
    fs::create_dir(&loop_dir).unwrap();
    fs::write(
        loop_dir.join("protocol.md"),
        "# Protocol\nSet STATUS done.\n",
    )
    .unwrap();
    fs::write(loop_dir.join("plan.md"), "## Stage 1\nDone.\n").unwrap();
    fs::write(
        loop_dir.join("yoke.conf"),
        "model gpt-5.5\nthinking xhigh\nallow .\n",
    )
    .unwrap();
    fs::write(loop_dir.join("notes.md"), "").unwrap();
    fs::write(loop_dir.join("guard-results.md"), "").unwrap();

    let mock_bin_dir = project.join("mock-bin");
    fs::create_dir(&mock_bin_dir).unwrap();
    let mock_omp = mock_bin_dir.join("omp");
    fs::write(&mock_omp, MOCK_OMP).unwrap();
    fs::set_permissions(&mock_omp, fs::Permissions::from_mode(0o755)).unwrap();

    let legacy_bin = mock_bin_dir.join(["cl", "aude"].concat());
    fs::write(
        &legacy_bin,
        "#!/usr/bin/env bash\necho 'legacy agent must not be invoked' >&2\nexit 99\n",
    )
    .unwrap();
    fs::set_permissions(&legacy_bin, fs::Permissions::from_mode(0o755)).unwrap();

    let original_path = std::env::var("PATH").unwrap_or_default();
    let test_path = format!("{}:{}", mock_bin_dir.display(), original_path);
    let output = Command::new(&yoke)
        .args(["run"])
        .current_dir(project)
        .env("PATH", &test_path)
        .output()
        .expect("yoke run");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "yoke should exit 0 using mock omp. exit={:?}\nstderr:\n{}",
        output.status.code(),
        stderr
    );

    let argv = fs::read_to_string(loop_dir.join(".omp-argv")).expect("mock omp argv");
    assert!(
        argv.contains("--mode\njson"),
        "OMP must run in JSON mode.\nargv:\n{}",
        argv
    );
    assert!(
        argv.contains("--auto-approve"),
        "OMP must auto-approve tools.\nargv:\n{}",
        argv
    );
    assert!(
        argv.contains("--model\ngpt-5.5"),
        "model directive must pass through to OMP.\nargv:\n{}",
        argv
    );
    assert!(
        argv.contains("--thinking\nxhigh"),
        "thinking directive must pass through to OMP.\nargv:\n{}",
        argv
    );
    assert!(
        argv.contains("-p\nRead .loop/protocol.md and follow its instructions."),
        "worker prompt must be passed after -p for OMP print mode.\nargv:\n{}",
        argv
    );
}
