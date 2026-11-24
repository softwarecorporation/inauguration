use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};

use crate::native_runtime::Value;
use crate::package_install::{
    INSTALLED_PACKAGE_METADATA, InstalledPackageMetadata, PackageExportBinding, PackageInvokeSpec,
};
use crate::package_lock::PackageLock;
use crate::package_manifest::{
    PackageManifest, load_package_manifest_from_source, resolve_dependency_install_path,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackageExportRuntime {
    pub install_path: String,
    #[serde(default)]
    pub params: Vec<String>,
    pub returns: String,
    pub invoke: PackageInvokeSpec,
}

pub(crate) const STRING_INVOKE: &str = "__in_package_invoke_string";
pub(crate) const STRING_QUOTE: &str = "__in_package_quote";
pub(crate) const STRING_CONCAT: &str = "__in_package_concat";

thread_local! {
    static INVOCATION_ERROR: RefCell<Option<String>> = const { RefCell::new(None) };
}

pub(crate) fn take_invocation_error() -> Option<String> {
    INVOCATION_ERROR.with(|error| error.borrow_mut().take())
}

pub(crate) fn record_invocation_error(message: String) {
    INVOCATION_ERROR.with(|error| {
        error.borrow_mut().get_or_insert(message);
    });
}

pub fn collect_package_exports_for_source(
    source_path: &Path,
) -> BTreeMap<String, PackageExportRuntime> {
    let Ok((root, manifest)) = load_package_manifest_from_source(source_path) else {
        return BTreeMap::new();
    };
    let lock = crate::package_lock::discover_package_lock(&root.root)
        .and_then(|lock_root| crate::package_lock::load_package_lock(&lock_root.lock_path).ok());
    collect_package_exports_for_manifest(&root.root, &manifest, lock.as_ref())
}

pub fn collect_package_exports_for_manifest(
    package_root: &Path,
    manifest: &PackageManifest,
    lock: Option<&PackageLock>,
) -> BTreeMap<String, PackageExportRuntime> {
    let mut exports = BTreeMap::new();
    for (dependency_key, dependency) in &manifest.dependencies {
        let Some(install_path) =
            resolve_dependency_install_path(package_root, dependency_key, dependency, lock)
        else {
            continue;
        };
        let metadata_path = install_path.join(INSTALLED_PACKAGE_METADATA);
        let Ok(source) = std::fs::read_to_string(&metadata_path) else {
            continue;
        };
        let Ok(metadata) = serde_json::from_str::<InstalledPackageMetadata>(&source) else {
            continue;
        };
        for binding in metadata.bindings {
            exports.insert(
                binding.symbol.clone(),
                PackageExportRuntime {
                    install_path: install_path.display().to_string(),
                    params: binding.params,
                    returns: binding.returns,
                    invoke: binding.invoke,
                },
            );
        }
    }
    exports
}

pub fn invoke_package_export(
    runtime: &PackageExportRuntime,
    args: &[Value],
) -> Result<Value, String> {
    if args.len() != runtime.params.len() {
        return Err(format!(
            "package export expects {} arguments, got {}",
            runtime.params.len(),
            args.len()
        ));
    }
    let strings = args
        .iter()
        .zip(&runtime.params)
        .map(|(argument, expected)| match (argument, expected.as_str()) {
            (Value::String(value), "string" | "String") => Ok(value),
            _ => Err(format!(
                "unsupported package argument for type `{expected}`"
            )),
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mut invoke = runtime.invoke.clone();
    if !strings.is_empty() {
        invoke.args.push(
            serde_json::to_string(&strings)
                .map_err(|err| format!("encode package arguments: {err}"))?,
        );
    }
    let install_path = PathBuf::from(&runtime.install_path);
    let output = run_invoke(&invoke, &install_path)?;
    match runtime.returns.as_str() {
        "string" => Ok(Value::String(output)),
        "int" => output
            .trim()
            .parse::<i64>()
            .map(Value::Int)
            .map_err(|err| format!("package export returned non-int `{output}`: {err}")),
        "void" => Ok(Value::Nil),
        other => Err(format!("unsupported package export return type `{other}`")),
    }
}

const ALLOWED_INVOKE_PROGRAMS: &[&str] = &["echo", "node", "python3", "cargo", "go", "true"];

pub fn run_invoke(spec: &PackageInvokeSpec, install_path: &Path) -> Result<String, String> {
    if !ALLOWED_INVOKE_PROGRAMS.contains(&spec.program.as_str()) {
        return Err(format!(
            "package export invoke program `{}` is not in the allowlist",
            spec.program
        ));
    }
    let output = Command::new(&spec.program)
        .args(&spec.args)
        .current_dir(install_path)
        .output()
        .map_err(|err| {
            format!(
                "failed to invoke package export via `{}` in {}: {err}",
                spec.program,
                install_path.display()
            )
        })?;
    if !output.status.success() {
        return Err(format!(
            "package export invoke `{}` failed in {}: {}",
            spec.program,
            install_path.display(),
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    String::from_utf8(output.stdout)
        .map_err(|err| format!("package export invoke returned non-utf8 stdout: {err}"))
}

pub fn binding_return_type(binding: &PackageExportBinding) -> Option<crate::core_ir::Typ> {
    match binding.returns.as_str() {
        "int" | "Int" => Some(crate::core_ir::Typ::Int),
        "string" | "String" => Some(crate::core_ir::Typ::String),
        "bool" | "Bool" => Some(crate::core_ir::Typ::Bool),
        "void" => Some(crate::core_ir::Typ::Void),
        _ => None,
    }
}

pub(crate) fn binding_params(
    binding: &PackageExportBinding,
) -> Option<Vec<(String, crate::core_ir::Typ)>> {
    binding
        .params
        .iter()
        .enumerate()
        .map(|(index, ty)| match ty.as_str() {
            "string" | "String" => Some((format!("argument_{index}"), crate::core_ir::Typ::String)),
            _ => None,
        })
        .collect()
}

pub(crate) fn lower_string_adapter(
    declaration: &mut crate::core_ir::Decl,
    runtime: &PackageExportRuntime,
) -> Result<bool, String> {
    use crate::core_ir::{Decl, Expr, Stmt, Typ};
    let Decl::Function {
        params, ret, body, ..
    } = declaration
    else {
        return Ok(false);
    };
    if *ret != Typ::String || params.iter().any(|(_, ty)| *ty != Typ::String) {
        return Ok(false);
    }
    let call = |name: &str, args| Expr::Call {
        callee: Box::new(Expr::Ident(name.into())),
        args,
    };
    let mut arguments = Expr::StringLit("[".into());
    for (index, (name, _)) in params.iter().enumerate() {
        if index != 0 {
            arguments = call(STRING_CONCAT, vec![arguments, Expr::StringLit(",".into())]);
        }
        arguments = call(
            STRING_CONCAT,
            vec![
                arguments,
                call(STRING_QUOTE, vec![Expr::Ident(name.clone())]),
            ],
        );
    }
    arguments = call(STRING_CONCAT, vec![arguments, Expr::StringLit("]".into())]);
    let descriptor = serde_json::to_string(runtime)
        .map_err(|err| format!("encode package invocation: {err}"))?;
    *body = vec![Stmt::Return(Some(call(
        STRING_INVOKE,
        vec![Expr::StringLit(descriptor), arguments],
    )))];
    Ok(true)
}

pub(crate) fn string_adapter_builtins() -> Vec<crate::core_ir::Decl> {
    use crate::core_ir::{Decl, Typ};
    [(STRING_INVOKE, 2), (STRING_QUOTE, 1), (STRING_CONCAT, 2)]
        .into_iter()
        .map(|(name, arity)| Decl::Function {
            name: name.into(),
            params: (0..arity)
                .map(|index| (format!("argument_{index}"), Typ::String))
                .collect(),
            ret: Typ::String,
            body: Vec::new(),
            type_params: Vec::new(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binding_return_type_mapping() {
        let cases = vec![
            ("int", Some(crate::core_ir::Typ::Int)),
            ("Int", Some(crate::core_ir::Typ::Int)),
            ("string", Some(crate::core_ir::Typ::String)),
            ("String", Some(crate::core_ir::Typ::String)),
            ("bool", Some(crate::core_ir::Typ::Bool)),
            ("Bool", Some(crate::core_ir::Typ::Bool)),
            ("void", Some(crate::core_ir::Typ::Void)),
            ("float", None),
        ];

        for (input, expected) in cases {
            let binding = PackageExportBinding {
                symbol: "test_sym".to_string(),
                params: Vec::new(),
                returns: input.to_string(),
                invoke: PackageInvokeSpec {
                    program: "test".to_string(),
                    args: vec![],
                },
            };
            assert_eq!(
                binding_return_type(&binding),
                expected,
                "failed for input: {}",
                input
            );
        }
    }

    #[test]
    fn invokes_echo_adapter() {
        let runtime = PackageExportRuntime {
            install_path: ".".to_string(),
            params: Vec::new(),
            returns: "string".to_string(),
            invoke: PackageInvokeSpec {
                program: "echo".to_string(),
                args: vec!["-n".to_string(), "ok".to_string()],
            },
        };
        let value = invoke_package_export(&runtime, &[]).expect("invoke");
        assert_eq!(value, Value::String("ok".to_string()));
    }

    #[test]
    fn rejects_shell_invoke_program() {
        let spec = PackageInvokeSpec {
            program: "sh".to_string(),
            args: vec!["-c".to_string(), "echo pwned".to_string()],
        };
        let err = run_invoke(&spec, Path::new(".")).expect_err("sh must not be allowlisted");
        assert!(err.contains("not in the allowlist"));
    }

    #[test]
    fn test_binding_return_type_mapping() {
        let cases = vec![
            ("int", Some(crate::core_ir::Typ::Int)),
            ("Int", Some(crate::core_ir::Typ::Int)),
            ("string", Some(crate::core_ir::Typ::String)),
            ("String", Some(crate::core_ir::Typ::String)),
            ("bool", Some(crate::core_ir::Typ::Bool)),
            ("Bool", Some(crate::core_ir::Typ::Bool)),
            ("void", Some(crate::core_ir::Typ::Void)),
            ("unknown", None),
        ];

        for (return_str, expected) in cases {
            let binding = PackageExportBinding {
                symbol: "test".to_string(),
                params: Vec::new(),
                returns: return_str.to_string(),
                invoke: PackageInvokeSpec {
                    program: "true".to_string(),
                    args: vec![],
                },
            };
            assert_eq!(binding_return_type(&binding), expected);
        }
    }
}
