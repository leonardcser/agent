use std::sync::atomic::{AtomicUsize, Ordering};
use url::Url;

static UA_COUNTER: AtomicUsize = AtomicUsize::new(0);

/// Return a glob pattern that matches all URLs on the same domain.
/// e.g. "https://docs.rs/foo/bar" -> "https://docs.rs/*"
pub fn domain_pattern(url: &str) -> Option<String> {
    let parsed = Url::parse(url).ok()?;
    let scheme = parsed.scheme();
    let host = parsed.host_str()?;
    Some(format!("{scheme}://{host}/*"))
}

const USER_AGENTS: &[&str] = &[
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36",
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36",
    "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36",
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:133.0) Gecko/20100101 Firefox/133.0",
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10.15; rv:133.0) Gecko/20100101 Firefox/133.0",
    "Mozilla/5.0 (X11; Linux x86_64; rv:133.0) Gecko/20100101 Firefox/133.0",
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.2 Safari/605.1.15",
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36 Edg/131.0.0.0",
    "Mozilla/5.0 (iPhone; CPU iPhone OS 18_2 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.2 Mobile/15E148 Safari/604.1",
    "Mozilla/5.0 (Linux; Android 14; SM-S911B) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Mobile Safari/537.36",
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0.0.0 Safari/537.36",
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0.0.0 Safari/537.36",
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:132.0) Gecko/20100101 Firefox/132.0",
    "Mozilla/5.0 (X11; Ubuntu; Linux x86_64; rv:132.0) Gecko/20100101 Firefox/132.0",
    "Mozilla/5.0 (iPad; CPU OS 18_2 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.2 Mobile/15E148 Safari/604.1",
    "Mozilla/5.0 (Linux; Android 14; Pixel 8) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Mobile Safari/537.36",
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/129.0.0.0 Safari/537.36",
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/129.0.0.0 Safari/537.36",
    "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/129.0.0.0 Safari/537.36",
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36 OPR/116.0.0.0",
];

pub fn next_user_agent() -> &'static str {
    // 80% round-robin, 20% random
    let idx = UA_COUNTER.fetch_add(1, Ordering::Relaxed);
    if idx.is_multiple_of(5) {
        // ~20%: pick pseudo-random based on counter mixing
        let mixed = idx.wrapping_mul(6364136223846793005).wrapping_add(1);
        USER_AGENTS[mixed % USER_AGENTS.len()]
    } else {
        USER_AGENTS[idx % USER_AGENTS.len()]
    }
}

/// Extract text content from HTML, stripping scripts/styles/etc.
pub fn extract_text(html: &str) -> String {
    use scraper::{Html, Selector};

    let doc = Html::parse_document(html);
    let skip = Selector::parse("script, style, noscript, iframe, object, embed, svg").unwrap();

    let mut text = String::new();
    fn collect(node: scraper::ElementRef, skip: &Selector, out: &mut String) {
        if skip.matches(&node) {
            return;
        }
        for child in node.children() {
            if let Some(t) = child.value().as_text() {
                let trimmed = t.trim();
                if !trimmed.is_empty() {
                    if !out.is_empty() {
                        out.push(' ');
                    }
                    out.push_str(trimmed);
                }
            } else if let Some(el) = scraper::ElementRef::wrap(child) {
                collect(el, skip, out);
            }
        }
    }

    if let Some(body) = doc
        .select(&Selector::parse("body").unwrap())
        .next()
    {
        collect(body, &skip, &mut text);
    }
    text
}

/// Convert HTML to markdown, stripping non-content elements first.
pub fn html_to_markdown(html: &str) -> String {
    use scraper::{Html, Selector};

    // Remove script/style/etc before conversion
    let doc = Html::parse_document(html);
    let remove = Selector::parse("script, style, noscript, iframe, object, embed, meta, link").unwrap();
    let mut cleaned = doc.html();
    for el in doc.select(&remove) {
        cleaned = cleaned.replace(&el.html(), "");
    }

    htmd::convert(&cleaned).unwrap_or_else(|_| extract_text(html))
}

/// Extract title from HTML.
pub fn extract_title(html: &str) -> Option<String> {
    use scraper::{Html, Selector};
    let doc = Html::parse_document(html);
    let sel = Selector::parse("title").unwrap();
    doc.select(&sel).next().map(|el| el.text().collect::<String>().trim().to_string())
}

/// Extract up to 50 deduplicated, canonicalized links from HTML.
pub fn extract_links(html: &str, base_url: &url::Url) -> Vec<String> {
    use scraper::{Html, Selector};
    use std::collections::HashSet;

    let doc = Html::parse_document(html);
    let sel = Selector::parse("a[href]").unwrap();
    let mut seen = HashSet::new();
    let mut links = Vec::new();

    for el in doc.select(&sel) {
        if links.len() >= 50 {
            break;
        }
        let Some(href) = el.value().attr("href") else {
            continue;
        };
        let href = href.trim();
        if href.is_empty()
            || href.starts_with("javascript:")
            || href.starts_with("mailto:")
            || href.starts_with("tel:")
            || href.starts_with('#')
        {
            continue;
        }
        let Ok(mut resolved) = base_url.join(href) else {
            continue;
        };
        resolved.set_fragment(None);
        let s = resolved.to_string();
        if seen.insert(s.clone()) {
            links.push(s);
        }
    }
    links
}

/// Truncate output to max lines/bytes, appending a note if truncated.
pub fn truncate_output(text: &str, max_lines: usize, max_bytes: usize) -> String {
    let mut lines: Vec<&str> = text.lines().collect();
    let mut truncated = false;

    if lines.len() > max_lines {
        lines.truncate(max_lines);
        truncated = true;
    }

    let mut result = lines.join("\n");
    if result.len() > max_bytes {
        result.truncate(max_bytes);
        truncated = true;
    }

    if truncated {
        result.push_str("\n\n[output truncated]");
    }
    result
}
