use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/packed-refs");
    if let Some(branch) = git(&["symbolic-ref", "--short", "HEAD"]) {
        println!("cargo:rerun-if-changed=.git/refs/heads/{branch}");
    }

    let package_version = std::env::var("CARGO_PKG_VERSION").unwrap_or_default();
    let head = git(&["rev-parse", "--short=6", "HEAD"]);
    let exact_tag = git(&["describe", "--tags", "--exact-match", "HEAD"]);

    let version = match (exact_tag, head) {
        (Some(tag), _) if !tag.is_empty() => tag,
        (_, Some(hash)) if !hash.is_empty() => {
            let base = git(&["describe", "--tags", "--abbrev=0"])
                .filter(|base| !base.is_empty())
                .unwrap_or(package_version);
            let dirty = git(&["status", "--porcelain"])
                .is_some_and(|status| !status.is_empty());
            let separator = if dirty { "^" } else { "-" };
            format!("{base}{separator}{hash}")
        }
        _ => package_version,
    };

    println!("cargo:rustc-env=GDU_DIFF_BUILD_VERSION={version}");
}
