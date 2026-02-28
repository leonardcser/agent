use super::web_cache;
use super::web_shared::next_user_agent;
use super::{str_arg, Tool, ToolResult};
use serde_json::Value;
use std::collections::HashMap;
use std::time::Duration;

pub struct WebSearchTool;

impl Tool for WebSearchTool {
    fn name(&self) -> &str {
        "web_search"
    }

    fn description(&self) -> &str {
        "Search the web using DuckDuckGo. Returns a list of results with titles, URLs, and descriptions."
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "The search query"
                }
            },
            "required": ["query"]
        })
    }

    fn needs_confirm(&self, args: &HashMap<String, Value>) -> Option<String> {
        let query = str_arg(args, "query");
        Some(format!("Search: {query}"))
    }

    fn execute(&self, args: &HashMap<String, Value>) -> ToolResult {
        let query = str_arg(args, "query");
        if query.is_empty() {
            return ToolResult {
                content: "Query cannot be empty".into(),
                is_error: true,
            };
        }

        let cache_key = format!("search:{query}");
        if let Some(cached) = web_cache::get(&cache_key) {
            return ToolResult {
                content: cached,
                is_error: false,
            };
        }

        let search_query = query.clone();
        let fetch_result = std::thread::spawn(move || {
            let ua = next_user_agent();
            let client = reqwest::blocking::Client::builder()
                .timeout(Duration::from_secs(20))
                .user_agent(ua)
                .redirect(reqwest::redirect::Policy::limited(10))
                .build()?;

            let encoded = url::form_urlencoded::Serializer::new(String::new())
                .append_pair("q", &search_query)
                .append_pair("kl", "us-en")
                .finish();

            let url = format!("https://html.duckduckgo.com/html/?{encoded}");

            let response = client
                .get(&url)
                .header("Accept", "text/html")
                .header("Accept-Language", "en-US,en;q=0.9")
                .send()?;

            if !response.status().is_success() {
                return Err(response.error_for_status().unwrap_err());
            }

            response.text()
        })
        .join();

        let html = match fetch_result {
            Ok(Ok(h)) => h,
            Ok(Err(e)) => {
                return ToolResult {
                    content: format!("Search failed: {e}"),
                    is_error: true,
                }
            }
            Err(_) => {
                return ToolResult {
                    content: "Search thread panicked".into(),
                    is_error: true,
                }
            }
        };

        let results = parse_ddg_results(&html);
        if results.is_empty() {
            return ToolResult {
                content: "No results found".into(),
                is_error: false,
            };
        }

        let mut output = String::new();
        for (i, r) in results.iter().enumerate() {
            output.push_str(&format!("{}. {}\n   {}\n", i + 1, r.title, r.link));
            if !r.description.is_empty() {
                output.push_str(&format!("   {}\n", r.description));
            }
            output.push('\n');
        }

        let output = output.trim_end().to_string();
        web_cache::put(&cache_key, &output);

        ToolResult {
            content: output,
            is_error: false,
        }
    }
}

struct SearchResult {
    title: String,
    link: String,
    description: String,
}

fn parse_ddg_results(html: &str) -> Vec<SearchResult> {
    use scraper::{Html, Selector};

    let doc = Html::parse_document(html);
    let result_sel = Selector::parse("div.result, div.web-result").unwrap();
    let title_sel = Selector::parse("a.result__a").unwrap();
    let snippet_sel = Selector::parse("a.result__snippet").unwrap();

    let mut results = Vec::new();

    for el in doc.select(&result_sel) {
        if results.len() >= 20 {
            break;
        }

        let Some(title_el) = el.select(&title_sel).next() else {
            continue;
        };

        let title: String = title_el.text().collect::<String>().trim().to_string();
        if title.is_empty() {
            continue;
        }

        let raw_href = title_el.value().attr("href").unwrap_or("").to_string();
        let link = extract_ddg_url(&raw_href);
        if link.is_empty() {
            continue;
        }

        let description = el
            .select(&snippet_sel)
            .next()
            .map(|s| s.text().collect::<String>().trim().to_string())
            .unwrap_or_default();

        results.push(SearchResult {
            title,
            link,
            description,
        });
    }

    results
}

fn extract_ddg_url(ddg_url: &str) -> String {
    if ddg_url.contains("uddg=") {
        if let Some(start) = ddg_url.find("uddg=") {
            let after = &ddg_url[start + 5..];
            let encoded = if let Some(end) = after.find('&') {
                &after[..end]
            } else {
                after
            };
            return url::form_urlencoded::parse(encoded.as_bytes())
                .next()
                .map(|(k, v)| {
                    if v.is_empty() {
                        k.to_string()
                    } else {
                        format!("{k}={v}")
                    }
                })
                .unwrap_or_default();
        }
    }

    if ddg_url.starts_with("http://") || ddg_url.starts_with("https://") {
        return ddg_url.to_string();
    }

    String::new()
}
