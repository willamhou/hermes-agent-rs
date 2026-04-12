use async_trait::async_trait;
use serde_json::json;

use hermes_core::{
    error::Result,
    message::ToolResult,
    tool::{Tool, ToolContext, ToolSchema},
};

pub struct SkillListTool;
pub struct SkillViewTool;
pub struct SkillManageTool;

#[async_trait]
impl Tool for SkillListTool {
    fn name(&self) -> &str {
        "skill_list"
    }

    fn toolset(&self) -> &str {
        "skills"
    }

    fn is_read_only(&self) -> bool {
        true
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: self.name().to_string(),
            description: "List all available skills with names and descriptions.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        }
    }

    async fn execute(&self, _args: serde_json::Value, ctx: &ToolContext) -> Result<ToolResult> {
        let Some(skills) = &ctx.skills else {
            return Ok(ToolResult::error("skill tools unavailable"));
        };

        let listed = skills.list().await?;
        let result = json!({
            "skills": listed,
        });
        Ok(ToolResult::ok(result.to_string()))
    }
}

#[async_trait]
impl Tool for SkillViewTool {
    fn name(&self) -> &str {
        "skill_view"
    }

    fn toolset(&self) -> &str {
        "skills"
    }

    fn is_read_only(&self) -> bool {
        true
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: self.name().to_string(),
            description: "View the full content of a skill by name.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string"}
                },
                "required": ["name"],
                "additionalProperties": false
            }),
        }
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> Result<ToolResult> {
        let Some(skills) = &ctx.skills else {
            return Ok(ToolResult::error("skill tools unavailable"));
        };
        let Some(name) = args.get("name").and_then(|v| v.as_str()) else {
            return Ok(ToolResult::error("missing required parameter: name"));
        };

        match skills.get(name).await? {
            Some(skill) => Ok(ToolResult::ok(json!({ "skill": skill }).to_string())),
            None => Ok(ToolResult::error(format!("skill not found: {name}"))),
        }
    }
}

#[async_trait]
impl Tool for SkillManageTool {
    fn name(&self) -> &str {
        "skill_manage"
    }

    fn toolset(&self) -> &str {
        "skills"
    }

    fn is_read_only(&self) -> bool {
        false
    }

    fn is_exclusive(&self) -> bool {
        true
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: self.name().to_string(),
            description: "Create, edit, or delete a skill.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["create", "edit", "delete"]},
                    "name": {"type": "string"},
                    "content": {"type": "string"}
                },
                "required": ["action", "name"],
                "additionalProperties": false
            }),
        }
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> Result<ToolResult> {
        let Some(skills) = &ctx.skills else {
            return Ok(ToolResult::error("skill tools unavailable"));
        };
        let Some(action) = args.get("action").and_then(|v| v.as_str()) else {
            return Ok(ToolResult::error("missing required parameter: action"));
        };
        let Some(name) = args.get("name").and_then(|v| v.as_str()) else {
            return Ok(ToolResult::error("missing required parameter: name"));
        };

        match action {
            "create" => {
                let Some(content) = args.get("content").and_then(|v| v.as_str()) else {
                    return Ok(ToolResult::error("missing required parameter: content"));
                };
                skills.create(name, content).await?;
            }
            "edit" => {
                let Some(content) = args.get("content").and_then(|v| v.as_str()) else {
                    return Ok(ToolResult::error("missing required parameter: content"));
                };
                skills.edit(name, content).await?;
            }
            "delete" => {
                skills.delete(name).await?;
            }
            other => {
                return Ok(ToolResult::error(format!("unsupported action: {other}")));
            }
        }

        Ok(ToolResult::ok(
            json!({
                "ok": true,
                "action": action,
                "name": name
            })
            .to_string(),
        ))
    }
}

inventory::submit! {
    hermes_tools::ToolRegistration { factory: || Box::new(SkillListTool) }
}

inventory::submit! {
    hermes_tools::ToolRegistration { factory: || Box::new(SkillViewTool) }
}

inventory::submit! {
    hermes_tools::ToolRegistration { factory: || Box::new(SkillManageTool) }
}
