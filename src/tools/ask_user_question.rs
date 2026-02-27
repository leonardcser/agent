use super::{Tool, ToolResult};
use serde_json::Value;
use std::collections::HashMap;

pub struct AskUserQuestionTool;

impl Tool for AskUserQuestionTool {
    fn name(&self) -> &str {
        "ask_user_question"
    }

    fn description(&self) -> &str {
        "Ask the user questions to gather preferences, clarify instructions, or get decisions on implementation choices. Present 1-4 questions with 2-4 options each."
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "questions": {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": 4,
                    "description": "Questions to ask the user (1-4 questions)",
                    "items": {
                        "type": "object",
                        "properties": {
                            "question": {
                                "type": "string",
                                "description": "The complete question to ask the user."
                            },
                            "header": {
                                "type": "string",
                                "description": "Very short label displayed as a tab (max 12 chars)."
                            },
                            "options": {
                                "type": "array",
                                "minItems": 2,
                                "maxItems": 4,
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "label": {
                                            "type": "string",
                                            "description": "Display text (1-5 words)."
                                        },
                                        "description": {
                                            "type": "string",
                                            "description": "Explanation of this option."
                                        }
                                    },
                                    "required": ["label", "description"]
                                }
                            },
                            "multiSelect": {
                                "type": "boolean",
                                "description": "Allow multiple selections."
                            }
                        },
                        "required": ["question", "header", "options", "multiSelect"]
                    }
                }
            },
            "required": ["questions"]
        })
    }

    fn execute(&self, _args: &HashMap<String, Value>) -> ToolResult {
        // The actual interaction is handled by the agent event loop.
        // This is a placeholder — the real result is injected by the main loop.
        ToolResult {
            content: "waiting for user response".into(),
            is_error: false,
        }
    }
}
