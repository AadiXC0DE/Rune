//! What a model family is trained to use.

use rune_core::config::{EditTool, EditToolSetting};

/// The edit tool a model is trained to use. `Patch` when the lower-cased id
/// contains `gpt-5`, `gpt-6`, `codex` or `gpt-oss`; otherwise `Replace`.
#[must_use]
pub fn edit_tool_for(model_id: &str) -> EditTool {
    let id = model_id.to_ascii_lowercase();
    if ["gpt-5", "gpt-6", "codex", "gpt-oss"]
        .iter()
        .any(|family| id.contains(family))
    {
        EditTool::Patch
    } else {
        EditTool::Replace
    }
}

/// Applies the user's setting over the model's default.
#[must_use]
pub fn edit_tool(setting: EditToolSetting, model_id: &str) -> EditTool {
    match setting {
        EditToolSetting::Auto => edit_tool_for(model_id),
        EditToolSetting::Replace => EditTool::Replace,
        EditToolSetting::Patch => EditTool::Patch,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edit_tool_follows_the_model_id() {
        for (id, expected) in [
            ("gpt-5", EditTool::Patch),
            ("gpt-5.1-codex", EditTool::Patch),
            ("GPT-6-Luna", EditTool::Patch),
            ("openai/gpt-oss-120b", EditTool::Patch),
            ("codex-mini-latest", EditTool::Patch),
            ("gpt-4.1", EditTool::Replace),
            ("claude-opus-5-5", EditTool::Replace),
            ("deepseek-chat", EditTool::Replace),
            ("", EditTool::Replace),
        ] {
            assert_eq!(edit_tool_for(id), expected, "{id}");
        }
    }

    #[test]
    fn the_setting_overrides_the_model() {
        assert_eq!(
            edit_tool(EditToolSetting::Replace, "gpt-5"),
            EditTool::Replace
        );
        assert_eq!(
            edit_tool(EditToolSetting::Patch, "claude-opus-5-5"),
            EditTool::Patch
        );
        assert_eq!(edit_tool(EditToolSetting::Auto, "gpt-5"), EditTool::Patch);
    }
}
