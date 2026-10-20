//! Locating transcript directories and naming the profile each belongs to.

use std::path::{Path, PathBuf};

/// A `projects` directory plus the profile label its rows are stored under.
#[derive(Debug, Clone, PartialEq)]
pub struct Source {
    pub profile: String,
    pub dir: PathBuf,
}

/// `~/.claude` is "default", `~/.claude-cadmium` is "cadmium", anything else is the
/// config dir's own name.
pub fn profile_label(config_dir: &Path) -> String {
    let name = config_dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    match name.as_str() {
        ".claude" | "claude" => "default".into(),
        n => n.strip_prefix(".claude-").unwrap_or(n).to_string(),
    }
}

/// A `--data-dir` may name either a config dir or its `projects` dir.
fn source_for(path: &Path) -> Source {
    let (config, dir) = if path.file_name().is_some_and(|n| n == "projects") {
        (path.parent().unwrap_or(path).to_path_buf(), path.to_path_buf())
    } else if path.join("projects").is_dir() {
        (path.to_path_buf(), path.join("projects"))
    } else {
        (path.to_path_buf(), path.to_path_buf())
    };
    Source { profile: profile_label(&config), dir }
}

/// Every `~/.claude*` config dir that has a `projects` dir, plus `~/.config/claude`.
fn all_profiles(home: &Path) -> Vec<Source> {
    let mut found: Vec<Source> = std::fs::read_dir(home)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n == ".claude" || n.starts_with(".claude-"))
                && p.join("projects").is_dir()
        })
        .map(|p| source_for(&p))
        .collect();
    let xdg = home.join(".config/claude");
    if xdg.join("projects").is_dir() {
        found.push(source_for(&xdg));
    }
    found.sort_by(|a, b| a.profile.cmp(&b.profile));
    found
}

/// Resolution order: `--data-dir`, then `--all-profiles`, then `$CLAUDE_CONFIG_DIR`,
/// then the Claude Code defaults.
pub fn resolve(data_dirs: &[PathBuf], all: bool, config_dir: Option<&str>, home: &Path) -> Vec<Source> {
    if !data_dirs.is_empty() {
        return data_dirs.iter().map(|p| source_for(p)).collect();
    }
    if all {
        return all_profiles(home);
    }
    if let Some(dir) = config_dir.filter(|d| !d.is_empty()) {
        return vec![source_for(Path::new(dir))];
    }
    [home.join(".claude"), home.join(".config/claude")]
        .iter()
        .filter(|p| p.join("projects").is_dir())
        .map(|p| source_for(p))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels() {
        assert_eq!(profile_label(Path::new("/h/.claude")), "default");
        assert_eq!(profile_label(Path::new("/h/.claude-cadmium")), "cadmium");
        assert_eq!(profile_label(Path::new("/h/.config/claude")), "default");
        assert_eq!(profile_label(Path::new("/x/other")), "other");
    }

    #[test]
    fn resolution_order() {
        let home = std::env::temp_dir().join(format!("cm-paths-{}", std::process::id()));
        for d in [".claude/projects", ".claude-work/projects", ".claude-empty"] {
            std::fs::create_dir_all(home.join(d)).unwrap();
        }
        let explicit = resolve(&[home.join(".claude-work/projects")], true, Some("/nope"), &home);
        assert_eq!(explicit, vec![Source { profile: "work".into(), dir: home.join(".claude-work/projects") }]);

        let all: Vec<_> = resolve(&[], true, None, &home).into_iter().map(|s| s.profile).collect();
        assert_eq!(all, ["default", "work"]);

        let env = resolve(&[], false, Some(home.join(".claude-work").to_str().unwrap()), &home);
        assert_eq!(env[0].dir, home.join(".claude-work/projects"));

        let fallback = resolve(&[], false, None, &home);
        assert_eq!(fallback[0].profile, "default");
        std::fs::remove_dir_all(&home).unwrap();
    }
}
