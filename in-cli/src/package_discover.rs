use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::package_install::{InstalledPackageMetadata, PackageExportBinding, PackageInvokeSpec};
use crate::package_ref::PackageRef;

pub const PACKAGE_ADAPTER_FILE: &str = "inauguration.adapter.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackageAdapterFile {
    pub exports: Vec<PackageAdapterExport>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackageAdapterExport {
    pub symbol: String,
    #[serde(default)]
    pub params: Vec<String>,
    pub returns: String,
    pub invoke: PackageInvokeSpec,
}

pub fn discover_installed_package(
    install_path: &Path,
    package_ref: &PackageRef,
    version: &str,
    registry: &str,
) -> Result<InstalledPackageMetadata, String> {
    let adapter_path = install_path.join(PACKAGE_ADAPTER_FILE);
    let bindings = if adapter_path.is_file() {
        let source = fs::read_to_string(&adapter_path)
            .map_err(|err| format!("read {}: {err}", adapter_path.display()))?;
        let adapter: PackageAdapterFile = serde_json::from_str(&source)
            .map_err(|err| format!("parse {}: {err}", adapter_path.display()))?;
        adapter
            .exports
            .into_iter()
            .map(|export| PackageExportBinding {
                symbol: export.symbol,
                params: export.params,
                returns: export.returns,
                invoke: export.invoke,
            })
            .collect()
    } else {
        discover_from_ecosystem_layout(install_path, package_ref)?
    };
    let exports = bindings
        .iter()
        .map(|binding| binding.symbol.clone())
        .collect();
    Ok(InstalledPackageMetadata {
        ecosystem: package_ref.ecosystem.clone(),
        name: package_ref.name.clone(),
        version: version.to_string(),
        registry: registry.to_string(),
        install_path: install_path.display().to_string(),
        exports,
        bindings,
    })
}

fn discover_from_ecosystem_layout(
    install_path: &Path,
    package_ref: &PackageRef,
) -> Result<Vec<PackageExportBinding>, String> {
    match package_ref.ecosystem.as_str() {
        "npm" => discover_npm_package(install_path, package_ref),
        "pypi" => discover_pypi_package(install_path, package_ref),
        "cargo" => discover_cargo_package(install_path, package_ref),
        "go" => discover_go_package(install_path, package_ref),
        _ => Ok(vec![fallback_binding(package_ref)]),
    }
}

fn discover_npm_package(
    install_path: &Path,
    package_ref: &PackageRef,
) -> Result<Vec<PackageExportBinding>, String> {
    let package_json = install_path.join("package.json");
    if !package_json.is_file() {
        return Ok(vec![fallback_binding(package_ref)]);
    }
    let symbol = format!("{}_greet", sanitize_symbol_component(&package_ref.name));
    let arity = npm_greeting_arity(&install_path.join("index.js"))?;
    Ok(vec![PackageExportBinding {
        symbol,
        params: vec!["string".into(); arity],
        returns: "string".to_string(),
        invoke: PackageInvokeSpec {
            program: "node".to_string(),
            args: vec![
                "-e".to_string(),
                if arity == 0 {
                    "const m=require('./index.js'); process.stdout.write(m.greet());".into()
                } else {
                    "const m=require('./index.js'); process.stdout.write(m.greet(...JSON.parse(process.argv[1])));".into()
                },
            ],
        },
    }])
}

// The existing greeting adapter exposes strings. Discover its required arity
// without executing package code during installation; richer exports use an adapter file.
fn npm_greeting_arity(path: &Path) -> Result<usize, String> {
    let Ok(source) = fs::read_to_string(path) else {
        return Ok(0);
    };
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_javascript::LANGUAGE.into())
        .map_err(|err| format!("initialize npm adapter parser: {err}"))?;
    let tree = parser
        .parse(&source, None)
        .ok_or_else(|| "parse npm adapter source".to_string())?;
    if tree.root_node().has_error() {
        return Err(format!(
            "invalid JavaScript adapter source {}",
            path.display()
        ));
    }
    let mut arity = None;
    let mut cursor = tree.root_node().walk();
    for statement in tree.root_node().named_children(&mut cursor) {
        let Some(assignment) = statement.named_child(0) else {
            continue;
        };
        if assignment.kind() != "assignment_expression" {
            continue;
        }
        let Some(left) = assignment.child_by_field_name("left") else {
            continue;
        };
        let export = left
            .utf8_text(source.as_bytes())
            .unwrap_or_default()
            .split_whitespace()
            .collect::<String>();
        if !matches!(export.as_str(), "exports.greet" | "module.exports.greet") {
            continue;
        }
        let function = assignment
            .child_by_field_name("right")
            .ok_or_else(|| "npm greeting export has no function".to_string())?;
        if !matches!(function.kind(), "arrow_function" | "function_expression") || arity.is_some() {
            return Err(
                "npm greeting requires one direct function export or an explicit adapter".into(),
            );
        }
        if function.child_by_field_name("parameter").is_some() {
            arity = Some(1);
        } else if let Some(parameters) = function.child_by_field_name("parameters") {
            let mut cursor = parameters.walk();
            let parameters: Vec<_> = parameters
                .named_children(&mut cursor)
                .filter(|parameter| parameter.kind() != "comment")
                .collect();
            if parameters
                .iter()
                .any(|parameter| parameter.kind() != "identifier")
            {
                return Err(
                    "npm greeting defaults, rest and destructuring require an explicit adapter"
                        .into(),
                );
            }
            arity = Some(parameters.len());
        } else {
            return Err("npm greeting requires explicit function parameters".into());
        }
    }
    Ok(arity.unwrap_or(0))
}

fn discover_pypi_package(
    install_path: &Path,
    package_ref: &PackageRef,
) -> Result<Vec<PackageExportBinding>, String> {
    let module = install_path.join("greet.py");
    if !module.is_file() {
        return Ok(vec![fallback_binding(package_ref)]);
    }
    let symbol = format!("{}_greet", sanitize_symbol_component(&package_ref.name));
    Ok(vec![PackageExportBinding {
        symbol,
        params: Vec::new(),
        returns: "string".to_string(),
        invoke: PackageInvokeSpec {
            program: "python3".to_string(),
            args: vec![
                "-c".to_string(),
                "from greet import greet; import sys; sys.stdout.write(greet())".to_string(),
            ],
        },
    }])
}

fn discover_cargo_package(
    install_path: &Path,
    package_ref: &PackageRef,
) -> Result<Vec<PackageExportBinding>, String> {
    let manifest = install_path.join("Cargo.toml");
    if !manifest.is_file() {
        return Ok(vec![fallback_binding(package_ref)]);
    }
    let symbol = format!("{}_greet", sanitize_symbol_component(&package_ref.name));
    Ok(vec![PackageExportBinding {
        symbol,
        params: Vec::new(),
        returns: "string".to_string(),
        invoke: PackageInvokeSpec {
            program: "cargo".to_string(),
            args: vec![
                "run".to_string(),
                "--quiet".to_string(),
                "--manifest-path".to_string(),
                manifest.display().to_string(),
            ],
        },
    }])
}

fn discover_go_package(
    install_path: &Path,
    package_ref: &PackageRef,
) -> Result<Vec<PackageExportBinding>, String> {
    let main_go = install_path.join("main.go");
    if !main_go.is_file() {
        return Ok(vec![fallback_binding(package_ref)]);
    }
    let symbol = format!("{}_greet", sanitize_symbol_component(&package_ref.name));
    Ok(vec![PackageExportBinding {
        symbol,
        params: Vec::new(),
        returns: "string".to_string(),
        invoke: PackageInvokeSpec {
            program: "go".to_string(),
            args: vec!["run".to_string(), ".".to_string()],
        },
    }])
}

fn fallback_binding(package_ref: &PackageRef) -> PackageExportBinding {
    PackageExportBinding {
        symbol: crate::package_install::export_symbol_for(package_ref),
        params: Vec::new(),
        returns: "void".to_string(),
        invoke: PackageInvokeSpec {
            program: "true".to_string(),
            args: Vec::new(),
        },
    }
}

fn sanitize_symbol_component(name: &str) -> String {
    name.chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

const PYPI_SITE_PACKAGES_DIR: &str = "inauguration-site-packages";

fn prepare_go_package(install_path: &Path) -> Result<(), String> {
    let invoke_dir = install_path.join("inauguration-invoke");
    if !invoke_dir.join("go.mod").is_file() {
        return Ok(());
    }
    let status = std::process::Command::new("go")
        .args(["mod", "tidy"])
        .current_dir(&invoke_dir)
        .status()
        .map_err(|err| format!("go mod tidy not available for go package: {err}"))?;
    if !status.success() {
        return Err(format!(
            "go mod tidy failed for package at {}",
            install_path.display()
        ));
    }
    Ok(())
}

fn prepare_pypi_package(install_path: &Path) -> Result<(), String> {
    let has_manifest = ["pyproject.toml", "setup.py"]
        .into_iter()
        .any(|name| install_path.join(name).is_file());
    if !has_manifest {
        return Ok(());
    }
    let target = install_path.join(PYPI_SITE_PACKAGES_DIR);
    fs::create_dir_all(&target)
        .map_err(|err| format!("create pypi site-packages {}: {err}", target.display()))?;
    let _ = std::process::Command::new("python3")
        .args([
            "-m",
            "pip",
            "install",
            "--quiet",
            "--disable-pip-version-check",
            "--upgrade",
            "--target",
        ])
        .arg(&target)
        .arg(".")
        .current_dir(install_path)
        .status();
    // ponytail: pip --upgrade so existing dir warnings don't block
    Ok(())
}

pub fn adapter_overlay_dir(package_root: &Path, dependency_key: &str) -> PathBuf {
    package_root.join("adapters").join(dependency_key)
}

pub fn apply_adapter_overlay(
    package_root: &Path,
    dependency_key: &str,
    install_path: &Path,
) -> Result<(), String> {
    let overlay = adapter_overlay_dir(package_root, dependency_key);
    if !overlay.is_dir() {
        return Ok(());
    }
    copy_overlay_tree(&overlay, install_path)
}

fn copy_overlay_tree(source: &Path, destination: &Path) -> Result<(), String> {
    for entry in
        fs::read_dir(source).map_err(|err| format!("read overlay {}: {err}", source.display()))?
    {
        let entry = entry.map_err(|err| format!("read overlay entry: {err}"))?;
        let file_type = entry
            .file_type()
            .map_err(|err| format!("overlay file type {}: {err}", entry.path().display()))?;
        let target = destination.join(entry.file_name());
        if file_type.is_dir() {
            fs::create_dir_all(&target)
                .map_err(|err| format!("create overlay dir {}: {err}", target.display()))?;
            copy_overlay_tree(&entry.path(), &target)?;
        } else if file_type.is_file() {
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)
                    .map_err(|err| format!("create overlay parent {}: {err}", parent.display()))?;
            }
            fs::copy(entry.path(), &target).map_err(|err| {
                format!(
                    "copy overlay {} -> {}: {err}",
                    entry.path().display(),
                    target.display()
                )
            })?;
        }
    }
    Ok(())
}

pub fn prepare_installed_package(install_path: &Path, ecosystem: &str) -> Result<(), String> {
    if ecosystem == "pypi" {
        return prepare_pypi_package(install_path);
    }
    if ecosystem == "go" {
        return prepare_go_package(install_path);
    }
    if ecosystem != "cargo" {
        return Ok(());
    }
    // Canonicalize to prevent path traversal outside the install directory
    let install_canon = install_path
        .canonicalize()
        .map_err(|e| format!("invalid install path {}: {e}", install_path.display()))?;
    let runner_manifest = install_canon.join("inauguration-runner").join("Cargo.toml");
    let manifest = if runner_manifest.is_file() {
        runner_manifest
    } else {
        let root_manifest = install_canon.join("Cargo.toml");
        if !root_manifest.is_file() {
            return Ok(());
        }
        root_manifest
    };
    // Validate manifest is within install directory
    let manifest_canon = manifest
        .canonicalize()
        .map_err(|e| format!("invalid manifest path {}: {e}", manifest.display()))?;
    if !manifest_canon.starts_with(&install_canon) {
        return Err(format!(
            "manifest outside install directory: {}",
            manifest_canon.display()
        ));
    }
    let status = std::process::Command::new("cargo")
        .args(["build", "--quiet", "--manifest-path"])
        .arg(&manifest_canon)
        .current_dir(&install_canon)
        .status()
        .map_err(|err| format!("cargo build not available for {ecosystem} package: {err}"))?;
    if !status.success() {
        return Err(format!(
            "cargo build failed for package at {}",
            install_canon.display()
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovers_adapter_exports() {
        let temp = std::env::temp_dir().join(format!(
            "package-discover-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        fs::create_dir_all(&temp).expect("temp");
        fs::write(
            temp.join(PACKAGE_ADAPTER_FILE),
            r#"{"exports":[{"symbol":"demo_greet","returns":"string","invoke":{"program":"echo","args":["ok"]}}]}"#,
        )
        .expect("adapter");
        let package_ref = PackageRef {
            ecosystem: "npm".to_string(),
            name: "demo".to_string(),
        };
        let metadata =
            discover_installed_package(&temp, &package_ref, "1.0.0", "path").expect("discover");
        assert_eq!(metadata.exports, vec!["demo_greet"]);
        assert_eq!(metadata.bindings[0].invoke.program, "echo");
        let _ = fs::remove_dir_all(temp);
    }
}
