use super::web_cache;
use super::web_shared::{
    domain_pattern, extract_links, extract_text, extract_title, html_to_markdown, next_user_agent,
    truncate_output,
};
use super::{str_arg, Tool, ToolResult};
use base64::Engine;
use serde_json::Value;
use std::collections::HashMap;
use std::time::Duration;

const MAX_RESPONSE_SIZE: usize = 5 * 1024 * 1024;
const DEFAULT_TIMEOUT: u64 = 30;
const MAX_TIMEOUT: u64 = 120;
const MAX_OUTPUT_LINES: usize = 2000;
const MAX_OUTPUT_BYTES: usize = 50 * 1024;

const IMAGE_TYPES: &[&str] = &[
    "image/png",
    "image/jpeg",
    "image/gif",
    "image/webp",
    "image/bmp",
    "image/tiff",
];

pub struct WebFetchTool;

impl Tool for WebFetchTool {
    fn name(&self) -> &str {
        "web_fetch"
    }

    fn description(&self) -> &str {
        "Fetch a URL and return its content as markdown (default), plain text, or raw HTML. Supports caching, link extraction, and image detection."
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "url": {
                    "type": "string",
                    "description": "The URL to fetch (must start with http:// or https://)"
                },
                "format": {
                    "type": "string",
                    "enum": ["markdown", "text", "html"],
                    "description": "Output format. Default: markdown"
                },
                "timeout": {
                    "type": "integer",
                    "description": "Timeout in seconds (max 120). Default: 30"
                }
            },
            "required": ["url"]
        })
    }

    fn needs_confirm(&self, args: &HashMap<String, Value>) -> Option<String> {
        let url = str_arg(args, "url");
        Some(url.to_string())
    }

    fn approval_pattern(&self, args: &HashMap<String, Value>) -> Option<String> {
        domain_pattern(&str_arg(args, "url"))
    }

    fn execute(&self, args: &HashMap<String, Value>) -> ToolResult {
        let url_str = str_arg(args, "url");
        let format = str_arg(args, "format");
        let format = if format.is_empty() {
            "markdown".to_string()
        } else {
            format
        };
        let timeout_secs = args
            .get("timeout")
            .and_then(|v| v.as_u64())
            .unwrap_or(DEFAULT_TIMEOUT)
            .min(MAX_TIMEOUT);

        if !url_str.starts_with("http://") && !url_str.starts_with("https://") {
            return ToolResult {
                content: "URL must start with http:// or https://".into(),
                is_error: true,
            };
        }

        let parsed_url = match url::Url::parse(&url_str) {
            Ok(u) => u,
            Err(e) => {
                return ToolResult {
                    content: format!("Invalid URL: {e}"),
                    is_error: true,
                }
            }
        };

        let cache_key = format!("fetch:{url_str}:{format}");
        if let Some(cached) = web_cache::get(&cache_key) {
            return ToolResult {
                content: cached,
                is_error: false,
            };
        }

        // Run blocking HTTP in a separate thread to avoid tokio runtime conflict
        let fetch_url = url_str.clone();
        let fetch_result = std::thread::spawn(move || {
            let timeout = Duration::from_secs(timeout_secs);
            let ua = next_user_agent();
            let client = reqwest::blocking::Client::builder()
                .timeout(timeout)
                .user_agent(ua)
                .redirect(reqwest::redirect::Policy::limited(10))
                .build()?;

            let response = client
                .get(&fetch_url)
                .header(
                    "Accept",
                    "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
                )
                .header("Accept-Language", "en-US,en;q=0.9")
                .send()?;

            // Cloudflare bot detection retry
            let response = if response.status().as_u16() == 403 {
                let cf = response
                    .headers()
                    .get("cf-mitigated")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("");
                if cf == "challenge" {
                    client
                        .get(&fetch_url)
                        .header("User-Agent", "agent")
                        .header("Accept", "text/html,application/xhtml+xml,*/*;q=0.8")
                        .header("Accept-Language", "en-US,en;q=0.9")
                        .send()?
                } else {
                    response
                }
            } else {
                response
            };

            Ok::<_, reqwest::Error>(response)
        })
        .join();

        let response = match fetch_result {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => {
                return ToolResult {
                    content: format!("Fetch failed: {e}"),
                    is_error: true,
                }
            }
            Err(_) => {
                return ToolResult {
                    content: "Fetch thread panicked".into(),
                    is_error: true,
                }
            }
        };

        let status = response.status();
        if !status.is_success() {
            return ToolResult {
                content: format!("HTTP {status}"),
                is_error: true,
            };
        }

        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_lowercase();

        // Image handling
        if IMAGE_TYPES.iter().any(|t| content_type.contains(t)) {
            let bytes = match response.bytes() {
                Ok(b) => b,
                Err(e) => {
                    return ToolResult {
                        content: format!("Failed to read image: {e}"),
                        is_error: true,
                    }
                }
            };
            let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
            let mime = content_type.split(';').next().unwrap_or("image/png").trim();
            return ToolResult {
                content: format!("![image](data:{mime};base64,{b64})"),
                is_error: false,
            };
        }

        if let Some(len) = response.content_length() {
            if len as usize > MAX_RESPONSE_SIZE {
                return ToolResult {
                    content: format!("Response too large: {len} bytes (max {MAX_RESPONSE_SIZE})"),
                    is_error: true,
                };
            }
        }

        let body = match response.text() {
            Ok(t) => t,
            Err(e) => {
                return ToolResult {
                    content: format!("Failed to read response: {e}"),
                    is_error: true,
                }
            }
        };

        if body.len() > MAX_RESPONSE_SIZE {
            return ToolResult {
                content: format!(
                    "Response too large: {} bytes (max {MAX_RESPONSE_SIZE})",
                    body.len()
                ),
                is_error: true,
            };
        }

        let is_html = content_type.contains("text/html") || content_type.contains("xhtml");
        let title = if is_html { extract_title(&body) } else { None };
        let links = if is_html {
            extract_links(&body, &parsed_url)
        } else {
            vec![]
        };

        let content = match format.as_str() {
            "text" => {
                if is_html {
                    extract_text(&body)
                } else {
                    body.clone()
                }
            }
            "html" => body.clone(),
            _ => {
                if is_html {
                    html_to_markdown(&body)
                } else {
                    body.clone()
                }
            }
        };

        let mut output = String::new();
        if let Some(t) = &title {
            output.push_str(&format!("# {t}\n\n"));
        }
        output.push_str(&content);
        if !links.is_empty() {
            output.push_str("\n\n## Links\n\n");
            for link in &links {
                output.push_str(&format!("- {link}\n"));
            }
        }

        let output = truncate_output(&output, MAX_OUTPUT_LINES, MAX_OUTPUT_BYTES);
        web_cache::put(&cache_key, &output);

        ToolResult {
            content: output,
            is_error: false,
        }
    }
}
