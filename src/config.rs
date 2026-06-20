use crate::types::AppConfig;
use anyhow::{Context, Result};
use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;

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

pub fn load_config() -> Result<Option<AppConfig>> {
    let path = config_path()?;
    if !path.exists() {
        return Ok(None);
    }
    let raw = fs::read_to_string(&path).with_context(|| format!("读取配置失败: {}", path.display()))?;
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
    let suffix: String = chars.iter().rev().take(4).collect::<Vec<_>>().into_iter().rev().collect();
    format!("{}****{}", prefix, suffix)
}

pub fn load_project_memory() -> Result<Option<String>> {
    let path = memory_path()?;
    if !path.exists() {
        return Ok(None);
    }
    let content = fs::read_to_string(&path).with_context(|| format!("读取项目记忆失败: {}", path.display()))?;
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
}
