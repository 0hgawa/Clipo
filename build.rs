use std::process::Command;

fn main() {
    // Bundle the gettext .po translations (lang/<code>/LC_MESSAGES/clipo.po)
    // into the binary so the UI switches language at runtime. No per-component
    // translation context — the .po are keyed by the plain English string.
    let cfg = slint_build::CompilerConfiguration::new()
        .with_bundled_translations("lang")
        .with_default_translation_context(slint_build::DefaultTranslationContext::None);
    slint_build::compile_with_config("ui/app.slint", cfg).expect("compile app.slint");
    println!("cargo:rerun-if-changed=lang");

    // Embed the brand icon as the Windows exe/taskbar/Explorer icon. Pure
    // compile-time resource linking — zero runtime cost.
    embed_resource::compile("app.rc", embed_resource::NONE)
        .manifest_required()
        .expect("embed app icon");
    println!("cargo:rerun-if-changed=app.rc");
    println!("cargo:rerun-if-changed=assets/icon.ico");
    println!("cargo:rerun-if-changed=assets/imageicon.ico");

    // Build identity for the About tab — short commit + commit date.
    let git = |args: &[&str]| {
        Command::new("git")
            .args(args)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string())
            .unwrap_or_default()
    };
    println!("cargo:rustc-env=CLIPO_COMMIT={}", git(&["rev-parse", "--short", "HEAD"]));
    println!(
        "cargo:rustc-env=CLIPO_COMMIT_DATE={}",
        git(&["log", "-1", "--format=%cd", "--date=short"])
    );
    // Re-run when HEAD moves so the badges stay current.
    println!("cargo:rerun-if-changed=.git/HEAD");
}
