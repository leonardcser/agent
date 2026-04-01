# Benchmarks

## SWE-bench Verified

[SWE-bench Verified](https://www.swebench.com/) is a curated subset of SWE-bench
containing 500 human-validated real-world GitHub issues from popular Python
repositories. Each task requires the agent to read the issue, navigate the
repository, and produce a patch that passes the project's test suite.

### Results

Model: **Qwen/Qwen3.5-27B**

| Agent                                               | Version | Resolved  | Date       |
| --------------------------------------------------- | ------- | --------- | ---------- |
| **smelt**                                           | 0.4.0   | **69.4%** | 2026-02-17 |
| [opencode](https://github.com/opencode-ai/opencode) | 1.3.9   | 67.2%     | 2026-02-17 |
