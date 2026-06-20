use crate::types::{AgentMode, AppConfig};
use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

pub fn config_dir() -> Result<PathBuf> {
    let home = dirs::home_dir().context("无法定位用户 home 目录")?;
    Ok(home.join(".yunzhi"))
}

pub fn config_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("config.toml"))
}

pub fn memory_path() -> Result<PathBuf> {
    Ok(std::env::current_dir()?.join(".yunzhi").join("memory.md"))
}

pub fn global_profiles_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("profiles.toml"))
}

pub fn project_profiles_path(cwd: &Path) -> PathBuf {
    cwd.join(".yunzhi").join("profiles.toml")
}

pub fn load_config() -> Result<Option<AppConfig>> {
    let path = config_path()?;
    if !path.exists() {
        return Ok(None);
    }
    let raw =
        fs::read_to_string(&path).with_context(|| format!("读取配置失败: {}", path.display()))?;
    let cfg = toml::from_str::<AppConfig>(&raw).context("解析 ~/.yunzhi/config.toml 失败")?;
    if cfg.api_key.trim().is_empty() {
        Ok(None)
    } else {
        Ok(Some(cfg))
    }
}

pub fn save_config(config: &AppConfig) -> Result<()> {
    let dir = config_dir()?;
    fs::create_dir_all(&dir).with_context(|| format!("创建配置目录失败: {}", dir.display()))?;
    let raw = toml::to_string_pretty(config).context("序列化配置失败")?;
    let path = config_path()?;
    fs::write(&path, raw).with_context(|| format!("写入配置失败: {}", path.display()))?;
    Ok(())
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct ProfilesFile {
    #[serde(default)]
    pub profiles: HashMap<String, ProfileConfig>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct ProfileConfig {
    pub persona: Option<String>,
    pub mode: Option<AgentMode>,
    pub model: Option<String>,
    pub max_tokens: Option<u32>,
    pub tools: Option<Vec<String>>,
}

pub fn load_profile(cwd: &Path, name: &str) -> Result<Option<ProfileConfig>> {
    let project_path = project_profiles_path(cwd);
    if let Some(profile) =
        read_profiles_file(&project_path)?.and_then(|file| file.profiles.get(name).cloned())
    {
        return Ok(Some(profile));
    }
    let global_path = global_profiles_path()?;
    Ok(read_profiles_file(&global_path)?.and_then(|file| file.profiles.get(name).cloned()))
}

fn read_profiles_file(path: &Path) -> Result<Option<ProfilesFile>> {
    if !path.exists() {
        return Ok(None);
    }
    let raw = fs::read_to_string(path)
        .with_context(|| format!("读取 profile 配置失败: {}", path.display()))?;
    let profiles = toml::from_str::<ProfilesFile>(&raw)
        .with_context(|| format!("解析 profile 配置失败: {}", path.display()))?;
    Ok(Some(profiles))
}

pub fn ensure_config_interactive() -> Result<AppConfig> {
    if let Some(config) = load_config()? {
        return Ok(config);
    }

    println!("首次运行需要配置云智 One API Key。");
    print!("请输入 API Key: ");
    io::stdout().flush()?;
    let mut api_key = String::new();
    io::stdin().read_line(&mut api_key)?;
    let api_key = api_key.trim().to_string();
    anyhow::ensure!(!api_key.is_empty(), "API Key 不能为空");
    let config = AppConfig { api_key };
    save_config(&config)?;
    println!("已保存到 {}", config_path()?.display());
    Ok(config)
}

pub fn masked_key(api_key: &str) -> String {
    let chars: Vec<char> = api_key.chars().collect();
    if chars.len() <= 8 {
        return "****".to_string();
    }
    let prefix: String = chars.iter().take(4).collect();
    let suffix: String = chars
        .iter()
        .rev()
        .take(4)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("{}****{}", prefix, suffix)
}

pub fn load_project_memory() -> Result<Option<String>> {
    let path = memory_path()?;
    if !path.exists() {
        return Ok(None);
    }
    let content = fs::read_to_string(&path)
        .with_context(|| format!("读取项目记忆失败: {}", path.display()))?;
    if content.trim().is_empty() {
        Ok(None)
    } else {
        Ok(Some(content))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_key() {
        assert_eq!(masked_key("sk-1234567890"), "sk-1****7890");
        assert_eq!(masked_key("short"), "****");
    }

    #[test]
    fn loads_project_profile() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".yunzhi")).unwrap();
        std::fs::write(
            dir.path().join(".yunzhi/profiles.toml"),
            "[profiles.rust]\npersona = \"Rust reviewer\"\nmode = \"agent\"\nmodel = \"custom-model\"\nmax_tokens = 2048\ntools = [\"read_file\", \"test_loop\"]\n",
        )
        .unwrap();

        let profile = load_profile(dir.path(), "rust").unwrap().unwrap();
        assert_eq!(profile.persona.as_deref(), Some("Rust reviewer"));
        assert_eq!(profile.mode, Some(AgentMode::Agent));
        assert_eq!(profile.model.as_deref(), Some("custom-model"));
        assert_eq!(profile.max_tokens, Some(2048));
        assert_eq!(profile.tools.unwrap(), vec!["read_file", "test_loop"]);
    }
}
