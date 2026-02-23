//! Build pipeline for `dioxus-cloudflare` workers.
//!
//! Automates `cargo build` → `wasm-bindgen` → shim generation in a single command.

use anyhow::{bail, Context, Result};
use clap::Parser;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Build pipeline for dioxus-cloudflare workers.
///
/// Runs cargo build, wasm-bindgen, and generates the JavaScript shim
/// that wrangler needs to deploy the worker.
#[derive(Parser)]
#[command(version, about)]
struct Cli {
    /// Crate to build (passed to `cargo -p`)
    #[arg(short, long)]
    package: String,

    /// Build in release mode
    #[arg(long)]
    release: bool,

    /// Output directory for wasm-bindgen artifacts and shim
    #[arg(long, default_value = "build/worker")]
    out_dir: PathBuf,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    fix_msvc_path();

    cargo_build(&cli.package, cli.release)?;
    wasm_bindgen(&cli.package, cli.release, &cli.out_dir)?;
    generate_shim(&cli.package, &cli.out_dir)?;

    eprintln!("build complete: {}", cli.out_dir.display());
    Ok(())
}

/// On Windows, prepend the MSVC `link.exe` directory to PATH so it takes
/// priority over Git's `/usr/bin/link.exe`.
#[cfg(target_os = "windows")]
fn fix_msvc_path() {
    // Only apply fix if the Git Bash link.exe conflict exists
    if !Path::new("/usr/bin/link.exe").exists() {
        return;
    }

    let Some(msvc_dir) = find_msvc_link_dir() else {
        return;
    };

    if let Ok(path) = std::env::var("PATH") {
        let new_path = format!("{};{path}", msvc_dir.display());
        // SAFETY: single-threaded CLI, no concurrent env access.
        // `set_var` is unsafe in Rust 2024+ due to potential data races,
        // but this runs before any threads are spawned.
        #[allow(unsafe_code)]
        // SAFETY: no other threads exist at this point
        unsafe {
            std::env::set_var("PATH", &new_path);
        }
        eprintln!("prepended MSVC link.exe dir to PATH");
    }
}

#[cfg(target_os = "windows")]
fn find_msvc_link_dir() -> Option<PathBuf> {
    let vs_root = Path::new(r"C:\Program Files\Microsoft Visual Studio");
    if !vs_root.exists() {
        return None;
    }

    let mut candidates: Vec<PathBuf> = Vec::new();

    // Walk: <vs_root>/<year>/<edition>/VC/Tools/MSVC/<version>/bin/Hostx64/x64/link.exe
    let Ok(years) = fs::read_dir(vs_root) else {
        return None;
    };
    for year in years.flatten() {
        let Ok(editions) = fs::read_dir(year.path()) else {
            continue;
        };
        for edition in editions.flatten() {
            let msvc_dir = edition.path().join(r"VC\Tools\MSVC");
            let Ok(versions) = fs::read_dir(&msvc_dir) else {
                continue;
            };
            for version in versions.flatten() {
                let link = version.path().join(r"bin\Hostx64\x64\link.exe");
                if link.exists() {
                    if let Some(parent) = link.parent() {
                        candidates.push(parent.to_path_buf());
                    }
                }
            }
        }
    }

    // Sort descending so the newest version wins
    candidates.sort();
    candidates.pop()
}

#[cfg(not(target_os = "windows"))]
fn fix_msvc_path() {
    // No-op on non-Windows platforms
}

/// Run `cargo build --target wasm32-unknown-unknown -p <crate> [--release]`.
fn cargo_build(package: &str, release: bool) -> Result<()> {
    eprintln!(
        "cargo build -p {package}{}",
        if release { " --release" } else { "" }
    );

    let mut cmd = Command::new("cargo");
    cmd.args(["build", "--target", "wasm32-unknown-unknown", "-p", package]);
    if release {
        cmd.arg("--release");
    }

    let status = cmd.status().context("failed to run cargo build")?;

    if !status.success() {
        bail!("cargo build failed with {status}");
    }
    Ok(())
}

/// Run `wasm-bindgen --out-dir <dir> --target web <wasm>`.
fn wasm_bindgen(package: &str, release: bool, out_dir: &Path) -> Result<()> {
    let profile = if release { "release" } else { "debug" };
    let crate_underscored = package.replace('-', "_");
    let wasm_path = PathBuf::from(format!(
        "target/wasm32-unknown-unknown/{profile}/{crate_underscored}.wasm"
    ));

    if !wasm_path.exists() {
        bail!(
            "wasm file not found: {}\nhint: ensure the crate has `crate-type = [\"cdylib\"]`",
            wasm_path.display()
        );
    }

    fs::create_dir_all(out_dir)
        .with_context(|| format!("failed to create output directory: {}", out_dir.display()))?;

    eprintln!("wasm-bindgen → {}", out_dir.display());

    let status = Command::new("wasm-bindgen")
        .args(["--out-dir"])
        .arg(out_dir)
        .args(["--target", "web"])
        .arg(&wasm_path)
        .status()
        .context(
            "failed to run wasm-bindgen\nhint: install with `cargo install wasm-bindgen-cli`",
        )?;

    if !status.success() {
        bail!("wasm-bindgen failed with {status}");
    }
    Ok(())
}

/// Read the generated `.d.ts` to detect Durable Object classes, then write `shim.mjs`.
fn generate_shim(package: &str, out_dir: &Path) -> Result<()> {
    let crate_underscored = package.replace('-', "_");
    let dts_path = out_dir.join(format!("{crate_underscored}.d.ts"));

    let dts = fs::read_to_string(&dts_path)
        .with_context(|| format!("failed to read {}", dts_path.display()))?;

    let do_classes = detect_durable_objects(&dts);

    let shim = build_shim(&crate_underscored, &do_classes);
    let shim_path = out_dir.join("shim.mjs");
    fs::write(&shim_path, &shim)
        .with_context(|| format!("failed to write {}", shim_path.display()))?;

    if do_classes.is_empty() {
        eprintln!("shim.mjs generated (no Durable Objects detected)");
    } else {
        eprintln!(
            "shim.mjs generated (Durable Objects: {})",
            do_classes.join(", ")
        );
    }
    Ok(())
}

/// Known `worker` crate internal classes that are NOT Durable Objects.
const WORKER_INTERNALS: &[&str] = &[
    "ContainerStartupOptions",
    "IntoUnderlyingByteSource",
    "IntoUnderlyingSink",
    "IntoUnderlyingSource",
    "MinifyConfig",
    "R2Range",
];

/// Parse the `.d.ts` file and return class names that look like Durable Objects.
///
/// A class is a Durable Object if:
/// 1. It has `export class <Name>` (not `private constructor()`)
/// 2. It has a public `constructor(state: any, env: any)`
/// 3. It is not in the known worker internals list
fn detect_durable_objects(dts: &str) -> Vec<String> {
    let mut result = Vec::new();
    let mut current_class: Option<String> = None;
    let mut has_do_constructor = false;

    // Flush the current class into `result` if it qualifies as a DO.
    let mut flush = |cls: &Option<String>, is_do: bool| {
        if let Some(name) = cls {
            if is_do && !WORKER_INTERNALS.contains(&name.as_str()) {
                result.push(name.clone());
            }
        }
    };

    for line in dts.lines() {
        let trimmed = line.trim();

        if let Some(name) = trimmed.strip_prefix("export class ") {
            flush(&current_class, has_do_constructor);

            let name = name
                .split_whitespace()
                .next()
                .unwrap_or_default()
                .to_string();
            current_class = Some(name);
            has_do_constructor = false;
            continue;
        }

        // Look for the DO constructor signature within the current class
        if current_class.is_some()
            && trimmed.contains("constructor(state:")
            && trimmed.contains("env:")
        {
            has_do_constructor = true;
        }

        // Private constructor disqualifies
        if current_class.is_some() && trimmed.contains("private constructor()") {
            has_do_constructor = false;
        }
    }

    // Flush the last class
    flush(&current_class, has_do_constructor);

    result
}

/// Build the contents of `shim.mjs`.
fn build_shim(crate_underscored: &str, do_classes: &[String]) -> String {
    let mut lines = Vec::new();

    // Import wasm module
    lines.push(format!(
        "import wasmModule from \"./{crate_underscored}_bg.wasm\";"
    ));

    // Import from JS bindings
    let mut imports = vec!["initSync".to_string(), "fetch as workerFetch".to_string()];
    for cls in do_classes {
        imports.push(cls.clone());
    }
    lines.push(format!(
        "import {{ {} }} from \"./{crate_underscored}.js\";",
        imports.join(", ")
    ));

    // Init call
    lines.push(String::new());
    lines.push("initSync({ module: wasmModule });".to_string());

    // Export DOs if any
    if !do_classes.is_empty() {
        lines.push(String::new());
        lines.push(format!("export {{ {} }};", do_classes.join(", ")));
    }

    // Default export
    lines.push("export default { fetch: workerFetch };".to_string());

    // Trailing newline
    lines.push(String::new());

    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_do_from_dts_snippet() {
        let dts = r#"
export class ContainerStartupOptions {
    private constructor();
    free(): void;
}

export class TestDo {
    free(): void;
    fetch(req: Request): Promise<any>;
    constructor(state: any, env: any);
}

export class WsDo {
    free(): void;
    constructor(state: any, env: any);
}
"#;
        let classes = detect_durable_objects(dts);
        assert_eq!(classes, vec!["TestDo", "WsDo"]);
    }

    #[test]
    fn shim_with_durable_objects() {
        let shim = build_shim("test_worker", &["TestDo".into(), "WsDo".into()]);
        assert!(shim.contains("import wasmModule from \"./test_worker_bg.wasm\";"));
        assert!(shim.contains("fetch as workerFetch, TestDo, WsDo"));
        assert!(shim.contains("export { TestDo, WsDo };"));
        assert!(shim.contains("export default { fetch: workerFetch };"));
    }

    #[test]
    fn shim_without_durable_objects() {
        let shim = build_shim("my_worker", &[]);
        assert!(!shim.contains("export {"));
        assert!(shim.contains("export default { fetch: workerFetch };"));
    }
}
