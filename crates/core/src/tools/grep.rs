use super::{read_walker, req_str, targets_hidden_path, Tool};
use crate::error::{Error, Result};
use async_trait::async_trait;
use globset::Glob;
use serde_json::{json, Value};

pub struct GrepTool;

#[async_trait]
impl Tool for GrepTool {
    fn name(&self) -> &'static str {
        "Grep"
    }

    fn parallelizable(&self) -> bool {
        true
    }

    fn description(&self) -> &'static str {
        "Search file contents for a regex pattern under a directory. Respects \
         .gitignore. Returns matching lines as `path:line:text`, one per line."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string", "description": "Regex pattern"},
                "path":    {"type": "string", "description": "Base directory (default: cwd)"},
                "glob":    {"type": "string", "description": "Optional file filter, e.g. '*.rs'"}
            },
            "required": ["pattern"]
        })
    }

    async fn call(&self, input: Value) -> Result<String> {
        let pattern = req_str(&input, "pattern")?;
        let raw_base = input.get("path").and_then(Value::as_str).unwrap_or(".");
        let base = crate::sandbox::Sandbox::check(raw_base)?;
        let glob_filter = input.get("glob").and_then(Value::as_str);

        // Explicit dot-path target (e.g. grep inside `.thclaws/sessions/`)
        // → descend past the default hidden/gitignore filters.
        let include_hidden = targets_hidden_path([raw_base, glob_filter.unwrap_or("")]);

        let re = regex::Regex::new(pattern).map_err(|e| Error::Tool(format!("regex: {e}")))?;

        let glob_matcher = glob_filter
            .map(|g| Glob::new(g).map(|g| g.compile_matcher()))
            .transpose()
            .map_err(|e| Error::Tool(format!("glob filter: {e}")))?;

        let mut results: Vec<String> = Vec::new();
        let walker = read_walker(&base, include_hidden).build();

        for entry in walker.flatten() {
            let path = entry.path();
            if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
                continue;
            }
            if let Some(m) = &glob_matcher {
                // Match the file name alone to avoid dir-path false matches.
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy())
                    .unwrap_or_default();
                if !m.is_match(name.as_ref()) {
                    continue;
                }
            }
            let Ok(contents) = std::fs::read_to_string(path) else {
                continue;
            };
            for (i, line) in contents.lines().enumerate() {
                if re.is_match(line) {
                    results.push(format!("{}:{}:{}", path.display(), i + 1, line));
                }
            }
        }
        results.sort();
        Ok(results.join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn setup_tree() -> tempfile::TempDir {
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("src/main.rs"),
            "fn main() {\n    println!(\"hello\");\n}\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("src/lib.rs"),
            "pub fn greet() -> String {\n    \"hello\".into()\n}\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("README.md"), "# hello world\n").unwrap();
        dir
    }

    #[tokio::test]
    async fn finds_matching_lines() {
        let dir = setup_tree();
        let out = GrepTool
            .call(json!({
                "pattern": "hello",
                "path": dir.path().to_string_lossy(),
            }))
            .await
            .unwrap();
        let lines: Vec<&str> = out.lines().collect();
        // 3 occurrences: main.rs:2, lib.rs:2, README.md:1
        assert_eq!(lines.len(), 3, "got: {out}");
        assert!(lines
            .iter()
            .any(|l| l.contains("main.rs:2:") && l.contains("hello")));
        assert!(lines
            .iter()
            .any(|l| l.contains("lib.rs:2:") && l.contains("hello")));
        assert!(lines.iter().any(|l| l.contains("README.md:1:")));
    }

    #[tokio::test]
    async fn regex_pattern_works() {
        let dir = setup_tree();
        let out = GrepTool
            .call(json!({
                "pattern": r"fn \w+\(\)",
                "path": dir.path().to_string_lossy(),
            }))
            .await
            .unwrap();
        let lines: Vec<&str> = out.lines().collect();
        // fn main() and fn greet()
        assert_eq!(lines.len(), 2, "got: {out}");
    }

    #[tokio::test]
    async fn glob_filter_restricts_files() {
        let dir = setup_tree();
        let out = GrepTool
            .call(json!({
                "pattern": "hello",
                "path": dir.path().to_string_lossy(),
                "glob": "*.md"
            }))
            .await
            .unwrap();
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("README.md:"));
    }

    #[tokio::test]
    async fn no_matches_returns_empty() {
        let dir = setup_tree();
        let out = GrepTool
            .call(json!({
                "pattern": "nosuchpattern",
                "path": dir.path().to_string_lossy(),
            }))
            .await
            .unwrap();
        assert_eq!(out, "");
    }

    #[tokio::test]
    async fn bad_regex_errors() {
        let err = GrepTool
            .call(json!({"pattern": "[unclosed", "path": "."}))
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("regex"));
    }

    #[tokio::test]
    async fn respects_gitignore() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join(".gitignore"), "ignored.txt\n").unwrap();
        std::fs::write(dir.path().join("ignored.txt"), "secret").unwrap();
        std::fs::write(dir.path().join("visible.txt"), "secret").unwrap();
        // Make it look like a git repo so ignore actually applies .gitignore.
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();

        let out = GrepTool
            .call(json!({
                "pattern": "secret",
                "path": dir.path().to_string_lossy(),
            }))
            .await
            .unwrap();
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 1, "got: {out}");
        assert!(lines[0].contains("visible.txt"));
    }
}
