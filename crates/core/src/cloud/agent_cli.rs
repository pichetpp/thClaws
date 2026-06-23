//! Headless agent packaging + validation (dev-plan/47.5).
//!
//! Exposes the canonical [`crate::cloud::pack`] / [`crate::cloud::manifest`]
//! machinery behind `thclaws agent pack` / `thclaws agent validate` so
//! scripts + CI never re-derive the strip rules or manifest fusion. The
//! exact same bytes `/cloud publish` uploads come out of `pack` here; the
//! only thing missing is the network upload.

use std::path::{Path, PathBuf};

use crate::cloud::manifest::Manifest;
use crate::cloud::pack;
use crate::config::AgentConfig;

/// Resolve agent identity (id/name/description/uuid) for packing —
/// READ-ONLY (unlike publish's `ensure_agent_identity`, this never
/// migrates or writes settings.json). Prefers `.thclaws/settings.json::agent`,
/// falls back to identity fields on `manifest.json`.
fn read_agent_identity(folder: &Path) -> Result<AgentConfig, String> {
    let settings_path = folder.join(".thclaws/settings.json");
    if let Ok(raw) = std::fs::read_to_string(&settings_path) {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) {
            if let Some(agent_v) = v.get("agent") {
                if let Ok(agent) = serde_json::from_value::<AgentConfig>(agent_v.clone()) {
                    if agent.id.is_some() && agent.name.is_some() && agent.description.is_some() {
                        return Ok(agent);
                    }
                }
            }
        }
    }
    let manifest_path = folder.join("manifest.json");
    let raw = std::fs::read_to_string(&manifest_path)
        .map_err(|e| format!("can't read manifest.json: {e}"))?;
    let v: serde_json::Value =
        serde_json::from_str(&raw).map_err(|e| format!("manifest.json: {e}"))?;
    let get = |k: &str| v.get(k).and_then(|x| x.as_str()).map(String::from);
    let (id, name, description) = (get("id"), get("name"), get("description"));
    if id.is_none() || name.is_none() || description.is_none() {
        return Err(
            ".thclaws/settings.json::agent.{id,name,description} required \
             (and not derivable from manifest.json)"
                .into(),
        );
    }
    Ok(AgentConfig {
        id,
        name,
        description,
        uuid: get("uuid"),
    })
}

/// Build the fused tarball (identity + catalog manifest) and write it to
/// `out` (defaults to `<folder>/<id>-<version>.tar.gz`). Returns the
/// output path + the pack result for reporting. Same bytes `/cloud
/// publish` would upload.
pub fn pack_to_file(
    folder: &Path,
    out: Option<PathBuf>,
) -> Result<(PathBuf, pack::PackResult), String> {
    let agent = read_agent_identity(folder)?;
    let fused = Manifest::fuse_for_publish(&agent, &folder.join("manifest.json"))?;
    let fused_json =
        serde_json::to_vec_pretty(&fused).map_err(|e| format!("serialize fused manifest: {e}"))?;
    let result = pack::pack(folder, Some(&fused_json))?;
    let out_path =
        out.unwrap_or_else(|| folder.join(format!("{}-{}.tar.gz", fused.id, fused.version)));
    std::fs::write(&out_path, &result.bytes)
        .map_err(|e| format!("writing {}: {e}", out_path.display()))?;
    Ok((out_path, result))
}

#[derive(Default)]
pub struct ValidateReport {
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
    pub info: Vec<String>,
}

impl ValidateReport {
    pub fn ok(&self) -> bool {
        self.errors.is_empty()
    }
}

/// Banned globals the workflow sandbox strips — a script using them
/// would fail at runtime, so flag them at validate time.
const WORKFLOW_BANNED: &[&str] = &["eval(", "Function(", "fetch(", "require(", "console."];

/// Offline pre-publish lint of an agent folder. Mirrors the checks the
/// publish endpoint enforces server-side (AGENTS.md present, manifest
/// validates + fuses, shell_execution sandboxed/none) plus authoring
/// niceties (subagent `output_schema` is valid JSON Schema, workflow
/// scripts don't use stripped globals). Never mutates the folder.
pub fn validate_folder(folder: &Path) -> ValidateReport {
    let mut r = ValidateReport::default();

    if !folder.join("AGENTS.md").is_file() {
        r.errors.push("missing AGENTS.md at the agent root".into());
    }
    if !folder.join("manifest.json").is_file() {
        r.errors
            .push("missing manifest.json at the agent root".into());
    }

    // Identity + manifest fusion (the publish-blocking checks).
    match read_agent_identity(folder) {
        Ok(agent) => match Manifest::fuse_for_publish(&agent, &folder.join("manifest.json")) {
            Ok(fused) => {
                r.info
                    .push(format!("manifest ok — {} v{}", fused.id, fused.version));
                let se = &fused.permissions.shell_execution;
                if se != "sandboxed" && se != "none" {
                    r.errors.push(format!(
                        "permissions.shell_execution must be 'sandboxed' or 'none' (got '{se}')"
                    ));
                }
            }
            Err(e) => r.errors.push(format!("manifest: {e}")),
        },
        Err(e) => r.errors.push(e),
    }

    // 48.3: MCP servers a subagent scopes to (`mcp:`) must be declared in the
    // manifest (so installers know what to connect); bundled skills must exist.
    let declared_mcp: std::collections::HashSet<String> =
        std::fs::read_to_string(folder.join("manifest.json"))
            .ok()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
            .and_then(|v| {
                v.get("requires")
                    .and_then(|r| r.get("mcp_servers"))
                    .cloned()
            })
            .and_then(|m| serde_json::from_value::<Vec<String>>(m).ok())
            .unwrap_or_default()
            .into_iter()
            .collect();
    let skills_dir = folder.join(".thclaws/skills");

    // Subagent defs: output_schema / input_schema must be valid JSON Schema.
    let agents_dir = folder.join(".thclaws/agents");
    if let Ok(entries) = std::fs::read_dir(&agents_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }
            let file = path.file_name().and_then(|s| s.to_str()).unwrap_or("?");
            match crate::agent_defs::AgentDefsConfig::parse_md_file(&path) {
                Some(def) => {
                    for (label, schema) in [
                        ("output_schema", &def.output_schema),
                        ("input_schema", &def.input_schema),
                    ] {
                        if let Some(s) = schema {
                            if jsonschema::validator_for(s).is_err() {
                                r.errors.push(format!(
                                    "subagent {file}: {label} is not a valid JSON Schema"
                                ));
                            }
                        }
                    }
                    // 47.2: writePaths globs must compile.
                    for pat in &def.write_paths {
                        if globset::Glob::new(pat).is_err() {
                            r.errors.push(format!(
                                "subagent {file}: writePaths glob '{pat}' is invalid"
                            ));
                        }
                    }
                    // 48.4: writePaths confines the file-write tools (Write/Edit/
                    // office) but NOT Bash — a role with both can write outside
                    // its lane via a shell command. Warn so the lever isn't read
                    // as a guarantee it can't keep (empty `tools` inherits Bash).
                    if !def.write_paths.is_empty()
                        && (def.tools.is_empty()
                            || def.tools.iter().any(|t| t.eq_ignore_ascii_case("bash")))
                    {
                        r.warnings.push(format!(
                            "subagent {file}: has writePaths AND Bash — writePaths confines Write/Edit \
                             to its globs but NOT Bash; bash.sandbox (default on) confines Bash writes \
                             to the workspace OS-level, but not to these specific globs"
                        ));
                    }
                    // 48.3: an MCP server the subagent scopes to must be declared.
                    for srv in &def.mcp {
                        if !declared_mcp.contains(srv) {
                            r.warnings.push(format!(
                                "subagent {file}: scopes to MCP server '{srv}' not in manifest.requires.mcp_servers"
                            ));
                        }
                    }
                    // 48.3: a skill the subagent scopes to must be bundled.
                    for sk in &def.skills {
                        if !skills_dir.join(sk).is_dir() {
                            r.warnings.push(format!(
                                "subagent {file}: skill '{sk}' not bundled under .thclaws/skills/"
                            ));
                        }
                    }
                }
                None => r
                    .warnings
                    .push(format!("subagent {file}: could not parse frontmatter")),
            }
        }
    }

    // Workflow scripts: flag stripped globals that would fail at runtime.
    let wf_dir = folder.join(".thclaws/workflows");
    if let Ok(entries) = std::fs::read_dir(&wf_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("js") {
                continue;
            }
            let file = path.file_name().and_then(|s| s.to_str()).unwrap_or("?");
            let Ok(src) = std::fs::read_to_string(&path) else {
                continue;
            };
            for banned in WORKFLOW_BANNED {
                if src.contains(banned) {
                    r.warnings.push(format!(
                        "workflow {file}: uses `{banned}` — stripped from the sandbox, will fail at runtime"
                    ));
                }
            }
        }
    }

    // 48.3: syntax-check the deterministic Python scripts the subagents drive.
    // The real work usually lives there, but validate was blind to it — a broken
    // script shipped green. Best-effort: skipped with a note when python3 is absent.
    let scripts_dir = folder.join(".thclaws/scripts");
    if let Ok(entries) = std::fs::read_dir(&scripts_dir) {
        let py: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("py"))
            .collect();
        if !py.is_empty() {
            match std::process::Command::new("python3")
                .arg("-m")
                .arg("py_compile")
                .args(&py)
                .output()
            {
                Ok(out) if out.status.success() => {
                    r.info
                        .push(format!("{} python script(s) compile", py.len()));
                }
                Ok(out) => {
                    let msg = String::from_utf8_lossy(&out.stderr);
                    let last = msg
                        .lines()
                        .rev()
                        .find(|l| !l.trim().is_empty())
                        .unwrap_or("")
                        .trim();
                    r.errors.push(format!("python script syntax error: {last}"));
                }
                Err(_) => r
                    .warnings
                    .push("skipped python script syntax check (python3 not on PATH)".into()),
            }
        }
    }

    // Pack as a final gate — surfaces any pack-time error + the shipped size.
    match read_agent_identity(folder)
        .and_then(|a| Manifest::fuse_for_publish(&a, &folder.join("manifest.json")))
        .and_then(|f| serde_json::to_vec_pretty(&f).map_err(|e| e.to_string()))
        .and_then(|j| pack::pack(folder, Some(&j)))
    {
        Ok(res) => r.info.push(format!(
            "packs cleanly — {} file(s), {} stripped, {:.1} KB",
            res.included.len(),
            res.stripped.len(),
            res.bytes.len() as f64 / 1024.0
        )),
        Err(e) => r.errors.push(format!("pack failed: {e}")),
    }

    r
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, rel: &str, body: &str) {
        let p = dir.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }

    const MIN_MANIFEST: &str =
        r#"{"version":"0.1.0","id":"demo","name":"Demo","description":"a demo agent"}"#;

    #[test]
    fn validate_warns_on_writepaths_with_bash() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "AGENTS.md", "# Demo\n");
        write(d.path(), "manifest.json", MIN_MANIFEST);
        write(
            d.path(),
            ".thclaws/agents/writer.md",
            "---\nname: writer\ntools: Read, Write, Bash\nwritePaths: out/**\n---\nwork\n",
        );
        let r = validate_folder(d.path());
        assert!(r.ok(), "warning only: {:?}", r.errors);
        assert!(
            r.warnings
                .iter()
                .any(|w| w.contains("writePaths") && w.contains("Bash")),
            "expected writePaths+Bash warning, got {:?}",
            r.warnings
        );
    }

    #[test]
    fn validate_warns_on_undeclared_mcp_and_missing_skill() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "AGENTS.md", "# Demo\n");
        // manifest declares NO mcp_servers
        write(d.path(), "manifest.json", MIN_MANIFEST);
        // subagent scopes to an MCP server + a skill that aren't present
        write(
            d.path(),
            ".thclaws/agents/worker.md",
            "---\nname: worker\nmcp: pinn-ai\nskills: brand-voice\n---\ndo work\n",
        );
        let r = validate_folder(d.path());
        assert!(r.ok(), "warnings only, not errors: {:?}", r.errors);
        assert!(
            r.warnings
                .iter()
                .any(|w| w.contains("pinn-ai") && w.contains("mcp_servers")),
            "expected undeclared-MCP warning, got {:?}",
            r.warnings
        );
        assert!(
            r.warnings
                .iter()
                .any(|w| w.contains("brand-voice") && w.contains("skills/")),
            "expected missing-skill warning, got {:?}",
            r.warnings
        );
    }

    #[test]
    fn validate_passes_minimal_agent_with_valid_schema() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "AGENTS.md", "# Demo\n");
        write(d.path(), "manifest.json", MIN_MANIFEST);
        // a subagent with a valid inline output_schema exercises the
        // parse + JSON-Schema-validate branch without erroring.
        write(
            d.path(),
            ".thclaws/agents/planner.md",
            "---\nname: planner\noutput_schema: {\"type\": \"object\"}\n---\nplan things\n",
        );
        let r = validate_folder(d.path());
        assert!(r.ok(), "expected ok, errors: {:?}", r.errors);
    }

    #[test]
    fn validate_flags_missing_agents_md() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "manifest.json", MIN_MANIFEST);
        let r = validate_folder(d.path());
        assert!(!r.ok());
        assert!(
            r.errors.iter().any(|e| e.contains("AGENTS.md")),
            "errors: {:?}",
            r.errors
        );
    }

    #[test]
    fn pack_to_file_writes_fused_tarball() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "AGENTS.md", "# Demo\n");
        write(d.path(), "manifest.json", MIN_MANIFEST);
        let (out, res) = pack_to_file(d.path(), None).unwrap();
        assert!(out.exists());
        assert!(res.included.iter().any(|f| f == "manifest.json"));
        assert!(res.included.iter().any(|f| f == "AGENTS.md"));
    }
}
