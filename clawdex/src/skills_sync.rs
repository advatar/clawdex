use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_yaml::{Mapping, Value as YamlValue};
use walkdir::WalkDir;

use crate::util::home_dir;

#[derive(Debug)]
pub struct SkillsSyncOptions {
    pub prefix: Option<String>,
    pub link: bool,
    pub dry_run: bool,
    pub user_dir: Option<PathBuf>,
    pub repo_dir: Option<PathBuf>,
    pub source_dir: Option<PathBuf>,
}

pub fn sync_skills(opts: SkillsSyncOptions) -> Result<()> {
    if opts.link && opts.prefix.is_some() {
        anyhow::bail!("--prefix cannot be used with --link (would mutate source skills)");
    }

    let source_dir = resolve_source_dir(opts.source_dir)?;
    let mut targets = Vec::new();

    let user_target = opts
        .user_dir
        .or_else(default_user_dir)
        .context("user skills dir not found")?;
    targets.push(user_target);

    if let Some(repo_dir) = opts.repo_dir {
        targets.push(repo_dir);
    }

    let skills = list_skills(&source_dir)?;
    if skills.is_empty() {
        anyhow::bail!("no skills found in {}", source_dir.display());
    }

    for target_root in targets {
        if !opts.dry_run {
            fs::create_dir_all(&target_root)
                .with_context(|| format!("create dir {}", target_root.display()))?;
        }

        for skill_dir in &skills {
            let skill_name = skill_dir
                .file_name()
                .and_then(|s| s.to_str())
                .context("skill name")?;
            let dest = target_root.join(skill_name);

            if opts.dry_run {
                println!("[clawdex] would sync {} -> {}", skill_dir.display(), dest.display());
                continue;
            }

            if dest.exists() {
                fs::remove_dir_all(&dest)
                    .with_context(|| format!("remove {}", dest.display()))?;
            }

            if opts.link {
                #[cfg(unix)]
                {
                    std::os::unix::fs::symlink(skill_dir, &dest)
                        .with_context(|| format!("symlink {}", dest.display()))?;
                }
                #[cfg(not(unix))]
                {
                    anyhow::bail!("--link is only supported on unix platforms");
                }
            } else {
                copy_dir(skill_dir, &dest)?;
                if let Some(prefix) = opts.prefix.as_deref() {
                    let skill_md = dest.join("SKILL.md");
                    if skill_md.exists() {
                        update_frontmatter(&skill_md, skill_name, prefix)?;
                    }
                } else {
                    let skill_md = dest.join("SKILL.md");
                    if skill_md.exists() {
                        ensure_frontmatter(&skill_md, skill_name)?;
                    }
                }
            }
        }
    }

    Ok(())
}

fn resolve_source_dir(source_dir: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(dir) = source_dir {
        return Ok(dir);
    }

    if let Ok(env) = std::env::var("OPENCLAW_SKILLS_DIR") {
        let path = PathBuf::from(env);
        if path.exists() {
            return Ok(path);
        }
    }

    if let Ok(env) = std::env::var("OPENCLAW_ROOT") {
        let path = PathBuf::from(env).join("skills");
        if path.exists() {
            return Ok(path);
        }
    }

    let cwd = std::env::current_dir().context("current dir")?;
    let candidate = cwd.join("openclaw").join("skills");
    if candidate.exists() {
        return Ok(candidate);
    }

    anyhow::bail!("unable to locate OpenClaw skills directory; pass --source-dir")
}

fn default_user_dir() -> Option<PathBuf> {
    let home = home_dir().ok()?;
    Some(home.join(".codex").join("skills").join("openclaw"))
}

fn list_skills(source_dir: &Path) -> Result<Vec<PathBuf>> {
    let mut skills = Vec::new();
    for entry in fs::read_dir(source_dir).with_context(|| {
        format!("read skills directory {}", source_dir.display())
    })? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() && path.join("SKILL.md").exists() {
            skills.push(path);
        }
    }
    Ok(skills)
}

fn copy_dir(src: &Path, dest: &Path) -> Result<()> {
    fs::create_dir_all(dest).with_context(|| format!("create dir {}", dest.display()))?;
    for entry in WalkDir::new(src).into_iter().filter_map(Result::ok) {
        let rel = entry.path().strip_prefix(src).unwrap_or(entry.path());
        let target = dest.join(rel);
        if entry.file_type().is_dir() {
            fs::create_dir_all(&target)
                .with_context(|| format!("create dir {}", target.display()))?;
        } else if entry.file_type().is_file() {
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("create dir {}", parent.display()))?;
            }
            fs::copy(entry.path(), &target)
                .with_context(|| format!("copy {} -> {}", entry.path().display(), target.display()))?;
        }
    }
    Ok(())
}

fn update_frontmatter(path: &Path, skill_name: &str, prefix: &str) -> Result<()> {
    let contents = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let (frontmatter, body) = split_frontmatter(&contents);
    let mut mapping = frontmatter.unwrap_or_else(Mapping::new);

    let prefixed_name = format!("{}{}", prefix, skill_name);
    set_yaml_string(&mut mapping, "name", &prefixed_name);
    set_yaml_string_if_missing(&mut mapping, "description", "OpenClaw skill");

    let yaml = serde_yaml::to_string(&mapping).context("serialize frontmatter")?;
    let updated = format!("---\n{}---\n{}", yaml, body);
    fs::write(path, updated).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

fn ensure_frontmatter(path: &Path, skill_name: &str) -> Result<()> {
    let contents = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let (frontmatter, body) = split_frontmatter(&contents);
    let mut mapping = frontmatter.unwrap_or_else(Mapping::new);

    set_yaml_string_if_missing(&mut mapping, "name", skill_name);
    set_yaml_string_if_missing(&mut mapping, "description", "OpenClaw skill");

    let yaml = serde_yaml::to_string(&mapping).context("serialize frontmatter")?;
    let updated = format!("---\n{}---\n{}", yaml, body);
    fs::write(path, updated).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

fn split_frontmatter(contents: &str) -> (Option<Mapping>, String) {
    let mut lines = contents.lines();
    let first = lines.next().unwrap_or("");
    if first.trim() != "---" {
        return (None, contents.to_string());
    }
    let mut yaml_lines = Vec::new();
    for line in lines.by_ref() {
        if line.trim() == "---" {
            break;
        }
        yaml_lines.push(line);
    }
    let rest = lines.collect::<Vec<&str>>().join("\n");
    let yaml_text = yaml_lines.join("\n");
    let mapping = serde_yaml::from_str::<Mapping>(&yaml_text).ok();
    (mapping, rest)
}

fn set_yaml_string(mapping: &mut Mapping, key: &str, value: &str) {
    mapping.insert(
        YamlValue::String(key.to_string()),
        YamlValue::String(value.to_string()),
    );
}

fn set_yaml_string_if_missing(mapping: &mut Mapping, key: &str, value: &str) {
    let yaml_key = YamlValue::String(key.to_string());
    if mapping.get(&yaml_key).is_none() {
        mapping.insert(yaml_key, YamlValue::String(value.to_string()));
    }
}
