//! Secret redaction for chat content.
//!
//! The policy is **redact-at-ingress**: data entering the conversation from
//! outside the model (user input, tool results, inter-agent messages) is
//! scrubbed once at the boundary, before it lands in history. Model-generated
//! content (assistant text, reasoning, tool-call arguments) is never touched.
//! This is the only layer preventing secrets from leaving the machine toward
//! the LLM API.
//!
//! Detection uses a layered approach sourced from gitleaks' battle-tested
//! patterns:
//! 1. **Known prefix patterns** — provider tokens with distinctive prefixes (near-zero false positives)
//! 2. **Structural patterns** — PEM keys, JWTs, database connection strings
//! 3. **Keyword proximity** — `password = "..."`, `secret: "..."`, etc.
//! 4. **Shannon entropy** — catch-all for unknown high-entropy strings
//!
//! Redacted values are replaced with type-labeled placeholders like
//! `[REDACTED:github_pat]` so the LLM can still reason about what kind of
//! secret was present without seeing the value.

use regex::Regex;
use std::sync::OnceLock;

/// Minimum token length for entropy-based detection. Kept high to avoid
/// swallowing ordinary long identifiers; real opaque secrets are longer still.
const ENTROPY_MIN_LEN: usize = 32;

/// Shannon entropy threshold. Tokens above this are flagged.
const ENTROPY_THRESHOLD: f64 = 4.5;

/// Format a redaction placeholder with a type label.
fn placeholder(label: &str) -> String {
    format!("[REDACTED:{label}]")
}

struct LabeledPattern {
    regex: Regex,
    label: &'static str,
}

struct Patterns {
    /// Known prefix patterns — highest confidence, near-zero false positives.
    prefix: Vec<LabeledPattern>,
    /// Structural patterns — PEM blocks, JWTs, connection strings.
    structural: Vec<LabeledPattern>,
    /// Keyword proximity — `password = "value"` style.
    keyword: Vec<Regex>,
}

fn patterns() -> &'static Patterns {
    static INSTANCE: OnceLock<Patterns> = OnceLock::new();
    INSTANCE.get_or_init(|| {
        // (regex, label) pairs sourced from gitleaks v8 rules.
        let prefix: Vec<(&str, &str)> = vec![
            // ── AWS ──────────────────────────────────────────────────────
            (r"\b(?:A3T[A-Z0-9]|AKIA|ASIA|ABIA|ACCA)[A-Z2-7]{16}\b", "aws_access_key"),
            (r"ABSK[A-Za-z0-9+/]{109,269}={0,2}", "aws_secret_key"),
            // ── Anthropic ────────────────────────────────────────────────
            (r"sk-ant-api03-[a-zA-Z0-9_\-]{93}AA", "anthropic_api_key"),
            (r"sk-ant-admin01-[a-zA-Z0-9_\-]{93}AA", "anthropic_admin_key"),
            // ── OpenAI ───────────────────────────────────────────────────
            (r"sk-(?:proj|svcacct|admin)-[A-Za-z0-9_-]{58,74}T3BlbkFJ[A-Za-z0-9_-]{58,74}\b", "openai_api_key"),
            (r"sk-[a-zA-Z0-9]{20}T3BlbkFJ[a-zA-Z0-9]{20}", "openai_api_key"),
            // ── GitHub ───────────────────────────────────────────────────
            (r"ghp_[0-9a-zA-Z]{36}", "github_pat"),
            (r"github_pat_\w{82}", "github_pat"),
            (r"gho_[0-9a-zA-Z]{36}", "github_oauth"),
            (r"(?:ghu|ghs)_[0-9a-zA-Z]{36}", "github_token"),
            (r"ghr_[0-9a-zA-Z]{36}", "github_refresh_token"),
            // ── GitLab ───────────────────────────────────────────────────
            (r"glpat-[\w\-]{20,}", "gitlab_pat"),
            (r"gldt-[0-9a-zA-Z_\-]{20}", "gitlab_deploy_token"),
            (r"glcbt-[0-9a-zA-Z]{1,5}_[0-9a-zA-Z_\-]{20}", "gitlab_cb_token"),
            (r"glptt-[0-9a-f]{40}", "gitlab_pipeline_trigger"),
            (r"GR1348941[\w\-]{20}", "gitlab_runner_token"),
            // ── Slack ────────────────────────────────────────────────────
            (r"xoxb-[0-9]{10,13}-[0-9]{10,13}[a-zA-Z0-9\-]*", "slack_bot_token"),
            (r"xox[pe]-(?:[0-9]{10,13}-){3}[a-zA-Z0-9\-]{28,34}", "slack_token"),
            (r"(?i)xapp-\d-[A-Z0-9]+-\d+-[a-z0-9]+", "slack_app_token"),
            (r"(?i)xoxe\.xox[bp]-\d-[A-Z0-9]{163,166}", "slack_token"),
            (r"(?i)xoxe-\d-[A-Z0-9]{146}", "slack_token"),
            (r"xox[ar]-(?:\d-)?[0-9a-zA-Z]{8,48}", "slack_token"),
            (r"xox[os]-\d+-\d+-\d+-[a-fA-F\d]+", "slack_token"),
            (r"xoxb-[0-9]{8,14}-[a-zA-Z0-9]{18,26}", "slack_bot_token"),
            // ── Stripe ───────────────────────────────────────────────────
            (r"(?:sk|rk)_(?:test|live|prod)_[a-zA-Z0-9]{10,99}", "stripe_key"),
            // ── GCP / Google ─────────────────────────────────────────────
            (r"AIza[\w\-]{35}", "google_api_key"),
            // ── Azure AD ─────────────────────────────────────────────────
            (r"[a-zA-Z0-9_~.]{3}\dQ~[a-zA-Z0-9_~.\-]{31,34}", "azure_ad_secret"),
            // ── SendGrid ─────────────────────────────────────────────────
            (r"SG\.(?i)[a-z0-9=_\-\.]{66}", "sendgrid_key"),
            // ── npm ──────────────────────────────────────────────────────
            (r"npm_[a-z0-9]{36}", "npm_token"),
            // ── PyPI ─────────────────────────────────────────────────────
            (r"pypi-AgEIcHlwaS5vcmc[\w\-]{50,}", "pypi_token"),
            // ── DigitalOcean ─────────────────────────────────────────────
            (r"do[pors]_v1_[a-f0-9]{64}", "digitalocean_token"),
            // ── Twilio ───────────────────────────────────────────────────
            (r"SK[0-9a-fA-F]{32}", "twilio_key"),
            // ── Shopify ──────────────────────────────────────────────────
            (r"shp(?:ss|at|ca|pa)_[a-fA-F0-9]{32}", "shopify_token"),
            // ── Heroku ───────────────────────────────────────────────────
            (r"HRKU-AA[0-9a-zA-Z_\-]{58}", "heroku_key"),
            // ── Hashicorp ────────────────────────────────────────────────
            // vault_service_token (90+) must come before vault_token (24+)
            // so the more specific pattern wins when both match.
            (r"hvs\.[a-zA-Z0-9_\-]{90,}", "vault_service_token"),
            (r"hvs\.[a-zA-Z0-9_\-]{24,}", "vault_token"),
            (r"(?i)[a-z0-9]{14}\.atlasv1\.[a-z0-9\-_=]{60,70}", "atlas_token"),
            // ── Doppler ──────────────────────────────────────────────────
            (r"dp\.pt\.(?i)[a-z0-9]{43}", "doppler_token"),
            // ── Databricks ───────────────────────────────────────────────
            (r"dapi[a-f0-9]{32}(?:-\d)?", "databricks_token"),
            // ── Mailgun ──────────────────────────────────────────────────
            (r"key-[a-f0-9]{32}", "mailgun_key"),
            (r"pubkey-[a-f0-9]{32}", "mailgun_pubkey"),
            // ── Discord ──────────────────────────────────────────────────
            (r"[MN][A-Za-z\d]{23,}\.[A-Za-z\d_\-]{6}\.[A-Za-z\d_\-]{27,}", "discord_token"),
            // ── Cloudflare ───────────────────────────────────────────────
            (r"v1\.0[a-f0-9]{37,40}", "cloudflare_key"),
            // ── 1Password ────────────────────────────────────────────────
            (r"ops_[a-zA-Z0-9]{43}", "onepassword_token"),
            // ── Confluent ────────────────────────────────────────────────
            (r"(?i)(?:CONFLUENT|confluent)[A-Za-z0-9_]*[a-zA-Z0-9]{16}", "confluent_key"),
            // ── Age encryption key ───────────────────────────────────────
            (r"AGE-SECRET-KEY-1[QPZRY9X8GF2TVDW0S3JN54KHCE6MUA7L]{58}", "age_secret_key"),
            // ── Coinbase ─────────────────────────────────────────────────
            (r"coinbase[a-zA-Z0-9]{30,}", "coinbase_key"),
            // ── Linear ───────────────────────────────────────────────────
            (r"lin_api_[a-zA-Z0-9]{40}", "linear_api_key"),
            // ── Postman ──────────────────────────────────────────────────
            (r"PMAK-[a-f0-9]{24}-[a-f0-9]{34}", "postman_key"),
            // ── Vault batch token ────────────────────────────────────────
            (r"hvb\.[a-zA-Z0-9_\-]{138,212}", "vault_batch_token"),
            // ── Grafana ──────────────────────────────────────────────────
            (r"glc_[A-Za-z0-9+/]{32,400}={0,2}", "grafana_cloud_token"),
            (r"glsa_[A-Za-z0-9]{32}_[A-Fa-f0-9]{8}", "grafana_sa_token"),
            // ── Planetscale ──────────────────────────────────────────────
            (r"pscale_tkn_[a-zA-Z0-9_\.\-]{43}", "planetscale_token"),
            (r"pscale_oauth_[a-zA-Z0-9_\.\-]{43}", "planetscale_oauth"),
            (r"pscale_pw_[a-zA-Z0-9_\.\-]{43}", "planetscale_password"),
            // ── Pulumi ───────────────────────────────────────────────────
            (r"pul-[a-f0-9]{40}", "pulumi_token"),
            // ── Prefect ──────────────────────────────────────────────────
            (r"pnu_[a-z0-9]{36}", "prefect_token"),
            // ── Supabase ─────────────────────────────────────────────────
            (r"sbp_[a-f0-9]{40}", "supabase_token"),
            // ── Telegram bot token ───────────────────────────────────────
            (r"[0-9]{5,16}:A[A-Za-z0-9_\-]{34}", "telegram_bot_token"),
            // ── Slack webhook URLs ───────────────────────────────────────
            (r"hooks\.slack\.com/(?:services|workflows|triggers)/[A-Za-z0-9+/]{43,56}", "slack_webhook"),
        ];

        let structural: Vec<(&str, &str)> = vec![
            // PEM private key blocks (full block matching)
            (r"(?i)-----BEGIN[ A-Z0-9_-]{0,100}PRIVATE KEY(?: BLOCK)?-----[\s\S-]*?KEY(?: BLOCK)?-----", "private_key"),
            // JWT tokens (3 base64url segments separated by dots)
            (r"ey[a-zA-Z0-9]{17,}\.ey[a-zA-Z0-9/\\_\-]{17,}\.(?:[a-zA-Z0-9/\\_\-]{10,}={0,2})?", "jwt"),
            // GCP service account JSON identifier
            (r#""type"\s*:\s*"service_account""#, "gcp_service_account"),
            // Database connection strings with embedded credentials
            (r"(?i)(?:postgres|mysql|mongodb|redis|amqp|mariadb|cockroachdb)(?:\+\w+)?://[^:\s]+:[^@\s]+@[^\s]+", "database_url"),
            // Generic connection strings (ADO.NET / JDBC style)
            (r"(?i)(?:Server|Data Source)=[^;]+;[^;]*Password=[^;\s]+", "connection_string"),
        ];

        // `token` and `credential` are intentionally omitted: they're too
        // generic (e.g. `access_token_url`, `auth_token_scope`), and real
        // provider tokens are caught precisely by the prefix layer above.
        let keyword: Vec<&str> = vec![
            // Quoted values after secret-like keys
            r#"(?i)(?:password|passwd|pwd|secret|api_?key|auth_?key|private_?key|access_?key|api_?secret|client_?secret)\s*[:=]\s*"[^"]{8,}""#,
            r"(?i)(?:password|passwd|pwd|secret|api_?key|auth_?key|private_?key|access_?key|api_?secret|client_?secret)\s*[:=]\s*'[^']{8,}'",
            // Unquoted values (single token, no whitespace)
            r#"(?i)(?:password|passwd|pwd|secret|api_?key|auth_?key|private_?key|access_?key|api_?secret|client_?secret)\s*[:=]\s*[^\s'"]{16,}"#,
        ];

        let compile_labeled =
            |patterns: &[(&str, &'static str)]| -> Vec<LabeledPattern> {
                patterns
                    .iter()
                    .map(|(p, l)| LabeledPattern {
                        regex: Regex::new(p).expect("redact: invalid hardcoded regex pattern"),
                        label: l,
                    })
                    .collect()
            };

        Patterns {
            prefix: compile_labeled(&prefix),
            structural: compile_labeled(&structural),
            keyword: keyword
                .iter()
                .map(|p| Regex::new(p).expect("redact: invalid hardcoded regex pattern"))
                .collect(),
        }
    })
}

/// A redaction range: byte start, byte end, and the type label.
struct RedactRange {
    start: usize,
    end: usize,
    label: &'static str,
}

/// Redact secrets in the given text, returning a new string with secrets
/// replaced by type-labeled placeholders like `[REDACTED:github_pat]`.
pub fn redact(input: &str) -> String {
    if input.is_empty() {
        return String::new();
    }

    let mut ranges: Vec<RedactRange> = Vec::new();
    let pats = patterns();

    // Layer 1 & 2: prefix and structural patterns
    for lp in pats.prefix.iter().chain(pats.structural.iter()) {
        for m in lp.regex.find_iter(input) {
            ranges.push(RedactRange {
                start: m.start(),
                end: m.end(),
                label: lp.label,
            });
        }
    }

    // Layer 3: keyword proximity — redact only the value part.
    // The label is derived from the matched keyword name.
    for re in pats.keyword.iter() {
        for caps in re.find_iter(input) {
            let matched = caps.as_str();
            // Find the value after the `=` or `:` separator.
            if let Some(sep_pos) = matched.find('=').or_else(|| matched.find(':')) {
                let value_start = caps.start() + sep_pos + 1;
                let value_str = &matched[sep_pos + 1..];
                let trimmed = value_str.trim_start();
                let trim_offset = value_str.len() - trimmed.len();
                let actual_start = value_start + trim_offset;

                // Strip surrounding quotes from the redaction range.
                let (final_start, final_end) =
                    if trimmed.starts_with('"') || trimmed.starts_with('\'') {
                        (actual_start + 1, caps.end() - 1)
                    } else {
                        (actual_start, caps.end())
                    };
                if final_end > final_start {
                    let value = &input[final_start..final_end];
                    if looks_like_url_or_path(value) {
                        continue;
                    }
                    // Extract the keyword name for the label.
                    let key_part = matched[..sep_pos].trim();
                    let label = keyword_label(key_part);
                    ranges.push(RedactRange {
                        start: final_start,
                        end: final_end,
                        label,
                    });
                }
            }
        }
    }

    // Layer 4: Shannon entropy
    for (start, end, _entropy) in entropy_tokens(input) {
        ranges.push(RedactRange {
            start,
            end,
            label: "secret",
        });
    }

    if ranges.is_empty() {
        return input.to_string();
    }

    // Merge overlapping ranges. When ranges overlap, keep the label of
    // the first (highest-confidence) match.
    ranges.sort_by_key(|r| r.start);
    let mut merged: Vec<RedactRange> = Vec::new();
    for r in ranges {
        if let Some(last) = merged.last_mut() {
            if r.start <= last.end {
                last.end = last.end.max(r.end);
                continue;
            }
        }
        merged.push(r);
    }

    // Build the redacted string.
    let mut result = String::with_capacity(input.len());
    let mut pos = 0;
    for r in &merged {
        result.push_str(&input[pos..r.start]);
        result.push_str(&placeholder(r.label));
        pos = r.end;
    }
    result.push_str(&input[pos..]);
    result
}

/// Map a keyword match (the part before `=` or `:`) to a static label.
fn keyword_label(key: &str) -> &'static str {
    let lower = key.to_ascii_lowercase();
    if lower.ends_with("password") || lower.ends_with("passwd") || lower.ends_with("pwd") {
        "password"
    } else if lower.ends_with("api_secret") || lower.ends_with("client_secret") {
        "client_secret"
    } else if lower.ends_with("secret") {
        "secret"
    } else if lower.ends_with("api_key") || lower.ends_with("apikey") {
        "api_key"
    } else if lower.ends_with("auth_key") || lower.ends_with("authkey") {
        "auth_key"
    } else if lower.ends_with("private_key") || lower.ends_with("privatekey") {
        "private_key"
    } else if lower.ends_with("access_key") || lower.ends_with("accesskey") {
        "access_key"
    } else {
        "secret"
    }
}

/// Heuristic: does this value look like a URL or file path?
/// Used to avoid redacting things like `auth_key: "https://.../oauth"` or
/// `access_key = /etc/creds/my-key.pem`.
fn looks_like_url_or_path(value: &str) -> bool {
    value.starts_with("http://")
        || value.starts_with("https://")
        || value.starts_with("file://")
        || value.starts_with('/')
        || value.starts_with("./")
        || value.starts_with("../")
        || value.starts_with("~/")
}

/// Compute Shannon entropy of a byte slice over its unique byte values.
fn shannon_entropy(data: &[u8]) -> f64 {
    if data.is_empty() {
        return 0.0;
    }
    let mut counts = [0u32; 256];
    for &b in data {
        counts[b as usize] += 1;
    }
    let len = data.len() as f64;
    counts
        .iter()
        .filter(|&&c| c > 0)
        .map(|&c| {
            let p = c as f64 / len;
            -p * p.log2()
        })
        .sum()
}

/// Does a token have at least two distinct character classes
/// (lowercase / uppercase / digit / symbol)?
fn has_mixed_char_classes(token: &str) -> bool {
    let mut classes = 0u8;
    for b in token.bytes() {
        let bit = if b.is_ascii_lowercase() {
            1
        } else if b.is_ascii_uppercase() {
            2
        } else if b.is_ascii_digit() {
            4
        } else {
            8
        };
        classes |= bit;
        if classes.count_ones() >= 2 {
            return true;
        }
    }
    false
}

/// UUID shape: 8-4-4-4-12 hex chars with dashes, case-insensitive.
fn is_uuid_shape(token: &str) -> bool {
    static UUID_RE: OnceLock<Regex> = OnceLock::new();
    let re = UUID_RE.get_or_init(|| {
        Regex::new(r"^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$")
            .unwrap()
    });
    re.is_match(token)
}

/// Extract high-entropy tokens from text. Returns (byte_start, byte_end, entropy).
fn entropy_tokens(input: &str) -> Vec<(usize, usize, f64)> {
    static TOKEN_RE: OnceLock<Regex> = OnceLock::new();
    let re = TOKEN_RE.get_or_init(|| {
        // Match contiguous non-whitespace tokens that look like potential secrets:
        // must contain mixed character classes to filter out prose.
        Regex::new(r#"[^\s=:"'`,;\{\}\[\]\(\)]{20,}"#).unwrap()
    });

    let mut results = Vec::new();
    for m in re.find_iter(input) {
        let token = m.as_str();
        if token.len() < ENTROPY_MIN_LEN {
            continue;
        }
        // Skip tokens that are all lowercase alpha (likely English words).
        if token.bytes().all(|b| b.is_ascii_lowercase() || b == b'-') {
            continue;
        }
        // Skip tokens that look like file paths.
        if token.starts_with('/') || token.starts_with("./") || token.contains("/../") {
            continue;
        }
        // Skip URLs (connection strings are caught by structural patterns).
        if token.starts_with("http://") || token.starts_with("https://") {
            continue;
        }
        // Skip hex-only strings: commit SHAs, content hashes, and similar
        // non-secret identifiers the model often needs to reason about.
        if token.bytes().all(|b| b.is_ascii_hexdigit()) {
            continue;
        }
        // Skip UUIDs: structured identifiers, not secrets.
        if is_uuid_shape(token) {
            continue;
        }
        // Require at least two character classes so ordinary snake_case or
        // PascalCase identifiers don't trip the entropy check.
        if !has_mixed_char_classes(token) {
            continue;
        }
        let entropy = shannon_entropy(token.as_bytes());
        if entropy >= ENTROPY_THRESHOLD {
            results.push((m.start(), m.end(), entropy));
        }
    }
    results
}

/// Redact secrets in a single message's content at ingress.
///
/// Only mutates `content`. `reasoning_content` and `tool_calls.arguments`
/// are left alone — they reflect the model's own prior thought and actions,
/// generated from already-redacted inputs.
pub fn redact_message(msg: &mut protocol::Message) {
    if let Some(ref mut content) = msg.content {
        redact_content(content);
    }
}

/// Redact secrets within a `Content` value in place.
pub fn redact_content(content: &mut protocol::Content) {
    match content {
        protocol::Content::Text(ref mut s) => {
            let redacted = redact(s);
            if redacted != *s {
                *s = redacted;
            }
        }
        protocol::Content::Parts(ref mut parts) => {
            for part in parts {
                if let protocol::ContentPart::Text { ref mut text } = part {
                    let redacted = redact(text);
                    if redacted != *text {
                        *text = redacted;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: check that a redacted result contains `[REDACTED:<label>]`.
    fn assert_redacted_with(result: &str, label: &str) {
        let expected = format!("[REDACTED:{label}]");
        assert!(
            result.contains(&expected),
            "expected {expected} in: {result}"
        );
    }

    #[test]
    fn no_secrets_unchanged() {
        let input = "Hello world, this is a normal message.";
        assert_eq!(redact(input), input);
    }

    #[test]
    fn empty_input() {
        assert_eq!(redact(""), "");
    }

    // ── AWS ──────────────────────────────────────────────────────────────

    #[test]
    fn aws_access_key_akia() {
        let input = "My key is AKIAIOSFODNN7EXAMPLE and that's it.";
        let result = redact(input);
        assert!(!result.contains("AKIAIOSFODNN7EXAMPLE"));
        assert_redacted_with(&result, "aws_access_key");
        assert!(result.starts_with("My key is"));
    }

    #[test]
    fn aws_access_key_asia() {
        let input = "temp creds ASIABCDEFGHIJKLMNOPQ";
        let result = redact(input);
        assert!(!result.contains("ASIABCDEFGHIJKLMNOPQ"));
        assert_redacted_with(&result, "aws_access_key");
    }

    // ── Anthropic ────────────────────────────────────────────────────────

    #[test]
    fn anthropic_api_key() {
        let fake_key = format!("sk-ant-api03-{}AA", "a".repeat(93));
        let input = format!("API key: {fake_key}");
        let result = redact(&input);
        assert!(!result.contains("sk-ant-api03"));
        assert_redacted_with(&result, "anthropic_api_key");
    }

    #[test]
    fn anthropic_admin_key() {
        let fake_key = format!("sk-ant-admin01-{}AA", "b".repeat(93));
        let input = format!("Admin: {fake_key}");
        let result = redact(&input);
        assert!(!result.contains("sk-ant-admin01"));
        assert_redacted_with(&result, "anthropic_admin_key");
    }

    // ── GitHub ───────────────────────────────────────────────────────────

    #[test]
    fn github_pat() {
        // ghp_ + 36 alphanumeric chars
        let input = "export GITHUB_TOKEN=ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghij";
        let result = redact(input);
        assert!(!result.contains("ghp_"));
        assert_redacted_with(&result, "github_pat");
    }

    #[test]
    fn github_fine_grained_pat() {
        let token = format!("github_pat_{}", "x".repeat(82));
        let input = format!("token: {token}");
        let result = redact(&input);
        assert!(!result.contains("github_pat_"));
        assert_redacted_with(&result, "github_pat");
    }

    #[test]
    fn github_refresh_token() {
        // ghr_ + 36 alphanumeric chars
        let input = "ghr_ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghij";
        let result = redact(input);
        assert!(!result.contains("ghr_"));
        assert_redacted_with(&result, "github_refresh_token");
    }

    // ── GitLab ───────────────────────────────────────────────────────────

    #[test]
    fn gitlab_pat() {
        let input = "glpat-abcdefghij0123456789";
        let result = redact(input);
        assert!(!result.contains("glpat-"));
        assert_redacted_with(&result, "gitlab_pat");
    }

    #[test]
    fn gitlab_deploy_token() {
        let input = "gldt-abcdefghij0123456789";
        let result = redact(input);
        assert!(!result.contains("gldt-"));
        assert_redacted_with(&result, "gitlab_deploy_token");
    }

    #[test]
    fn gitlab_pipeline_trigger() {
        let input = "glptt-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let result = redact(input);
        assert!(!result.contains("glptt-"));
        assert_redacted_with(&result, "gitlab_pipeline_trigger");
    }

    #[test]
    fn gitlab_runner_registration() {
        let input = "GR1348941abcdefghijklmnopqrst";
        let result = redact(input);
        assert!(!result.contains("GR1348941"));
        assert_redacted_with(&result, "gitlab_runner_token");
    }

    // ── Slack ────────────────────────────────────────────────────────────

    #[test]
    fn slack_bot_token() {
        let input = "Use token xoxb-1234567890-1234567890-abcdefghij to auth.";
        let result = redact(input);
        assert!(!result.contains("xoxb-"));
        assert_redacted_with(&result, "slack_bot_token");
    }

    // ── Stripe ───────────────────────────────────────────────────────────

    #[test]
    fn stripe_live_key() {
        let key = format!("sk_live_{}", "a1b2c3".repeat(5));
        let input = format!("{key} is my stripe key");
        let result = redact(&input);
        assert!(!result.contains("sk_live_"));
        assert_redacted_with(&result, "stripe_key");
    }

    #[test]
    fn stripe_prod_key() {
        let input = format!("rk_prod_{}", "A1B2C3".repeat(5));
        let result = redact(&input);
        assert!(!result.contains("rk_prod_"));
        assert_redacted_with(&result, "stripe_key");
    }

    // ── GCP / Google ─────────────────────────────────────────────────────

    #[test]
    fn google_api_key() {
        let input = format!(
            "AIza{}",
            "Sy0aX1bY2c".repeat(4).chars().take(35).collect::<String>()
        );
        let result = redact(&input);
        assert!(!result.contains("AIza"));
        assert_redacted_with(&result, "google_api_key");
    }

    #[test]
    fn gcp_service_account_json() {
        let input = r#"Found {"type": "service_account", "project_id": "foo"}"#;
        let result = redact(input);
        assert!(!result.contains(r#""type": "service_account""#));
        assert_redacted_with(&result, "gcp_service_account");
    }

    // ── SendGrid ─────────────────────────────────────────────────────────

    #[test]
    fn sendgrid_key() {
        let key = format!("SG.{}", "a".repeat(66));
        let input = format!("key: {key}");
        let result = redact(&input);
        assert!(!result.contains("SG."));
        assert_redacted_with(&result, "sendgrid_key");
    }

    // ── Shopify ──────────────────────────────────────────────────────────

    #[test]
    fn shopify_access_token() {
        let input = format!("shpat_{}", "aa11bb22".repeat(4));
        let result = redact(&input);
        assert!(!result.contains("shpat"));
        assert_redacted_with(&result, "shopify_token");
    }

    // ── Hashicorp ────────────────────────────────────────────────────────

    #[test]
    fn vault_service_token() {
        // hvs. + 90+ chars to match vault_service_token (not the shorter vault_token)
        let token = format!("hvs.{}", "aB1".repeat(35));
        let input = format!("VAULT_TOKEN={token}");
        let result = redact(&input);
        assert!(!result.contains("hvs."));
        assert_redacted_with(&result, "vault_service_token");
    }

    // ── Structural ───────────────────────────────────────────────────────

    #[test]
    fn jwt_token() {
        let header = base64_url_encode(r#"{"alg":"HS256","typ":"JWT"}"#);
        let payload = base64_url_encode(r#"{"sub":"1234567890","name":"John"}"#);
        let sig = "SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c";
        let jwt = format!("{header}.{payload}.{sig}");
        let input = format!("Bearer {jwt}");
        let result = redact(&input);
        assert!(!result.contains(&jwt));
        assert_redacted_with(&result, "jwt");
    }

    #[test]
    fn pem_private_key() {
        let input =
            "-----BEGIN RSA PRIVATE KEY-----\nMIIEow...base64data...\n-----END RSA PRIVATE KEY-----";
        let result = redact(input);
        assert!(!result.contains("-----BEGIN RSA PRIVATE KEY-----"));
        assert_redacted_with(&result, "private_key");
    }

    #[test]
    fn postgres_connection_string() {
        let input = "DATABASE_URL=postgres://admin:s3cretP4ss@db.example.com:5432/mydb";
        let result = redact(input);
        assert!(!result.contains("s3cretP4ss"));
        assert_redacted_with(&result, "database_url");
    }

    #[test]
    fn mysql_connection_string() {
        let input = "mysql://root:hunter2@localhost:3306/db";
        let result = redact(input);
        assert!(!result.contains("hunter2"));
        assert_redacted_with(&result, "database_url");
    }

    // ── Keyword proximity ────────────────────────────────────────────────

    #[test]
    fn keyword_password_quoted() {
        let input = r#"config.password = "mySuperSecret123""#;
        let result = redact(input);
        assert!(!result.contains("mySuperSecret123"));
        assert_redacted_with(&result, "password");
        // The key name should still be visible.
        assert!(result.contains("password"));
    }

    #[test]
    fn keyword_api_key_unquoted() {
        let input = "API_KEY=myCustomKeyValue1234567890abcdef";
        let result = redact(input);
        assert!(!result.contains("myCustomKeyValue1234567890abcdef"));
        assert_redacted_with(&result, "api_key");
    }

    #[test]
    fn keyword_single_quoted() {
        let input = "secret: 'myVeryLongSecretValue123'";
        let result = redact(input);
        assert!(!result.contains("myVeryLongSecretValue123"));
        assert!(result.contains("secret"));
        assert_redacted_with(&result, "secret");
    }

    #[test]
    fn keyword_token_not_redacted_when_no_prefix() {
        // Generic `token = "..."` is too noisy to redact on its own;
        // real provider tokens are caught by the prefix layer.
        let input = r#"token = "aLongTokenValue12345678""#;
        assert_eq!(redact(input), input);
    }

    #[test]
    fn keyword_credential_not_redacted() {
        let input = r#"credential: "someCredentialValue123""#;
        assert_eq!(redact(input), input);
    }

    #[test]
    fn keyword_value_url_not_redacted() {
        // An `api_key` whose value is a URL should not be redacted —
        // it's almost certainly a reference, not a secret.
        let input = r#"api_key_url = "https://example.com/path/to/key/endpoint""#;
        assert_eq!(redact(input), input);
    }

    #[test]
    fn keyword_value_path_not_redacted() {
        let input = "private_key = /etc/ssl/private/server.key";
        assert_eq!(redact(input), input);
    }

    // ── Multiple secrets ─────────────────────────────────────────────────

    #[test]
    fn multiple_secrets_in_one_string() {
        let input = "AWS=AKIAIOSFODNN7EXAMPLE and GitHub=ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghij";
        let result = redact(input);
        assert!(!result.contains("AKIAIOSFODNN7EXAMPLE"));
        assert!(!result.contains("ghp_"));
        assert_redacted_with(&result, "aws_access_key");
        assert_redacted_with(&result, "github_pat");
    }

    // ── False positive avoidance ─────────────────────────────────────────

    #[test]
    fn normal_code_not_redacted() {
        let input = r#"fn main() { let x = 42; println!("{}", x); }"#;
        assert_eq!(redact(input), input);
    }

    #[test]
    fn short_password_not_redacted() {
        // password values under 8 chars should NOT trigger keyword detection.
        let input = r#"password = "short""#;
        assert_eq!(redact(input), input);
    }

    #[test]
    fn file_paths_not_redacted() {
        let input = "/usr/local/bin/something-with-a-long-name";
        assert_eq!(redact(input), input);
    }

    #[test]
    fn urls_not_redacted() {
        let input = "Visit https://example.com/very/long/path/to/some/resource";
        assert_eq!(redact(input), input);
    }

    #[test]
    fn git_sha_not_redacted() {
        let input = "commit abc123def456789";
        assert_eq!(redact(input), input);
    }

    #[test]
    fn full_git_sha_not_redacted() {
        // 40-char SHA-1 — previously caught by entropy layer.
        let input = "commit df2a1bc489ae7b30f1c89e0a5fc2e3d98b77c456";
        assert_eq!(redact(input), input);
    }

    #[test]
    fn sha256_hash_not_redacted() {
        let input = "sha256: e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        assert_eq!(redact(input), input);
    }

    #[test]
    fn uuid_not_redacted() {
        let input = "session 550e8400-e29b-41d4-a716-446655440000 started";
        assert_eq!(redact(input), input);
    }

    #[test]
    fn long_mixed_identifier_not_redacted() {
        // CamelCase identifier with digits/underscores — common in code,
        // not a secret. Would previously trip the entropy check.
        let input = "call fn CamelCaseIdentifierWith_Mixed_Case_123 here";
        assert_eq!(redact(input), input);
    }

    // ── Entropy ──────────────────────────────────────────────────────────

    #[test]
    fn shannon_entropy_calculation() {
        // Uniform distribution of 256 symbols -> max entropy = 8.0
        let uniform: Vec<u8> = (0..=255).collect();
        let e = shannon_entropy(&uniform);
        assert!((e - 8.0).abs() < 0.01);

        // All same byte -> 0 entropy
        let same = vec![0u8; 100];
        assert_eq!(shannon_entropy(&same), 0.0);
    }

    // ── Message redaction ────────────────────────────────────────────────

    #[test]
    fn redact_message_mutates_content() {
        let mut msg = protocol::Message::user(protocol::Content::text(
            "my key is ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghij",
        ));
        redact_message(&mut msg);
        let text = msg.content.as_ref().unwrap().text_content();
        assert!(!text.contains("ghp_"));
        assert_redacted_with(&text, "github_pat");
    }

    #[test]
    fn redact_message_leaves_reasoning_alone() {
        let secret = "ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghij";
        let reasoning = format!("I should probably use {secret} here.");
        let mut msg = protocol::Message::assistant(
            Some(protocol::Content::text("ok")),
            Some(reasoning.clone()),
            None,
        );
        redact_message(&mut msg);
        assert_eq!(msg.reasoning_content.as_deref(), Some(reasoning.as_str()));
    }

    #[test]
    fn redact_message_leaves_tool_call_args_alone() {
        let args = r#"{"path":"/tmp/ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghij.log"}"#;
        let call = protocol::ToolCall::new(
            "call_1".to_string(),
            protocol::FunctionCall {
                name: "read_file".to_string(),
                arguments: args.to_string(),
            },
        );
        let mut msg = protocol::Message::assistant(None, None, Some(vec![call]));
        redact_message(&mut msg);
        let tc = &msg.tool_calls.as_ref().unwrap()[0];
        assert_eq!(tc.function.arguments, args);
    }

    #[test]
    fn redact_message_leaves_clean_content() {
        let mut msg = protocol::Message::user(protocol::Content::text("Hello, how are you?"));
        redact_message(&mut msg);
        assert_eq!(
            msg.content.as_ref().unwrap().text_content(),
            "Hello, how are you?"
        );
    }

    fn base64_url_encode(input: &str) -> String {
        use base64::Engine;
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(input)
    }
}
