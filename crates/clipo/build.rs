use std::process::Command;

fn main() {
    let commit = run_git(&["rev-parse", "--short=8", "HEAD"]).unwrap_or_else(|| "dev".into());
    let date = run_git(&["log", "-1", "--format=%cs", "HEAD"]).unwrap_or_else(|| "unknown".into());
    println!("cargo:rustc-env=CLIPO_GIT_COMMIT={commit}");
    println!("cargo:rustc-env=CLIPO_COMMIT_DATE={date}");
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/index");

    tauri_build::build();
}

fn run_git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    out.status.success().then(|| {
        String::from_utf8(out.stdout)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    })?
}
