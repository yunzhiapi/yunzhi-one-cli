use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;
use std::str::FromStr;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Message {
    pub role: Role,
    pub content: Vec<ContentBlock>,
}

impl Message {
    pub fn user(text: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: vec![ContentBlock::text(text)],
        }
    }

    pub fn assistant(text: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: vec![ContentBlock::text(text)],
        }
    }

    pub fn system(text: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            content: vec![ContentBlock::text(text)],
        }
    }

    pub fn tool_result(
        tool_call_id: impl Into<String>,
        content: impl Into<String>,
        is_error: bool,
    ) -> Self {
        Self {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: tool_call_id.into(),
                content: content.into(),
                is_error,
            }],
        }
    }

    pub fn text(&self) -> String {
        self.content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        is_error: bool,
    },
}

impl ContentBlock {
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text { text: text.into() }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub input: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolOutput {
    pub content: String,
    pub is_error: bool,
}

impl ToolOutput {
    pub fn ok(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
        }
    }

    pub fn error(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AppConfig {
    pub api_key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

#[derive(Debug, Clone)]
pub enum StreamEvent {
    TextDelta(String),
    ToolUse(ToolCall),
    MessageStop,
}

#[derive(Debug, Clone)]
pub struct AgentOptions {
    pub dangerously_skip_permissions: bool,
    pub model: String,
    pub max_tokens: u32,
    pub mode: AgentMode,
    pub profile_name: Option<String>,
    pub persona: Option<String>,
    pub tool_allowlist: Option<Vec<String>>,
}

impl Default for AgentOptions {
    fn default() -> Self {
        Self {
            dangerously_skip_permissions: false,
            model: crate::llm::DEFAULT_MODEL.to_string(),
            max_tokens: 4096,
            mode: AgentMode::Agent,
            profile_name: None,
            persona: None,
            tool_allowlist: None,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum AgentMode {
    Chat,
    PlanAct,
    Entanglement,
    Agent,
    Team,
    Analyze,
}

impl AgentMode {
    pub const ALL: [Self; 6] = [
        Self::Chat,
        Self::PlanAct,
        Self::Entanglement,
        Self::Agent,
        Self::Team,
        Self::Analyze,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Chat => "chat",
            Self::PlanAct => "plan-act",
            Self::Entanglement => "entanglement",
            Self::Agent => "agent",
            Self::Team => "team",
            Self::Analyze => "analyze",
        }
    }
}

impl fmt::Display for AgentMode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for AgentMode {
    type Err = String;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "chat" => Ok(Self::Chat),
            "plan-act" | "plan&act" | "plan_act" | "planact" => Ok(Self::PlanAct),
            "entanglement" => Ok(Self::Entanglement),
            "agent" => Ok(Self::Agent),
            "team" => Ok(Self::Team),
            "analyze" | "analyse" => Ok(Self::Analyze),
            _ => Err(format!(
                "未知模式: {raw}。可选: {}",
                Self::ALL
                    .iter()
                    .map(|mode| mode.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_agent_modes() {
        assert_eq!("chat".parse::<AgentMode>().unwrap(), AgentMode::Chat);
        assert_eq!("plan&act".parse::<AgentMode>().unwrap(), AgentMode::PlanAct);
        assert_eq!("plan_act".parse::<AgentMode>().unwrap(), AgentMode::PlanAct);
        assert_eq!("team".parse::<AgentMode>().unwrap(), AgentMode::Team);
        assert!("unknown".parse::<AgentMode>().is_err());
    }

    #[test]
    fn displays_kebab_case_modes() {
        assert_eq!(AgentMode::PlanAct.to_string(), "plan-act");
        assert_eq!(AgentMode::Entanglement.to_string(), "entanglement");
    }
}
