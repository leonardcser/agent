#!/usr/bin/env bash
#
# refactor/check.sh — drift checks for refactor/ docs.
#
# Run by hand at session boundaries (start of session, before / after a phase
# lands). Not wired as a pre-commit. Exits non-zero on failure so it composes
# with `&&` in scripts.
#
# Checks:
#   1. puml validates (plantuml -checkonly)
#   2. SVG age vs puml age — warn if puml is newer (regenerate)
#   3. Every "P0".."P9" referenced in STATUS exists as a header in REFACTOR
#   4. Every Rust file path mentioned in INVENTORY exists in the repo, OR the
#      row's Fate is `deleted` / starts with `to-` / `tui::` (capability
#      destination)
#   5. INVENTORY row counts vs `find crates/<crate>/src -name '*.rs' | wc -l`
#      (warn on drift, don't fail — file moves cause transient mismatch)
#   6. STATUS "Last update" date — warn if older than 2 weeks
#   7. No leaked "engine::permissions" / "engine::Permissions" references in
#      REFACTOR / ARCHITECTURE / STATUS (engine should be policy-free in target)
#
# Exit code:
#   0 — all hard checks passed (warnings may be emitted)
#   1 — at least one hard check failed
#
# Usage:
#   refactor/check.sh           # run all checks
#   refactor/check.sh --quiet   # only print failures

set -uo pipefail

cd "$(dirname "$0")/.." || exit 2

QUIET=0
[[ ${1:-} == "--quiet" ]] && QUIET=1

PASS=0
FAIL=0
WARN=0

ok()   { (( QUIET )) || printf '  \033[32m✓\033[0m %s\n' "$*"; PASS=$((PASS+1)); }
fail() { printf '  \033[31m✗\033[0m %s\n' "$*"; FAIL=$((FAIL+1)); }
warn() { printf '  \033[33m!\033[0m %s\n' "$*"; WARN=$((WARN+1)); }

section() { (( QUIET )) || printf '\n\033[1m%s\033[0m\n' "$*"; }

README=refactor/README.md
REFACTOR=refactor/REFACTOR.md
STATUS=refactor/STATUS.md
INVENTORY=refactor/INVENTORY.md
FEATURES=refactor/FEATURES.md
ARCH=refactor/ARCHITECTURE.md
DECISIONS=refactor/DECISIONS.md
TRACE=refactor/TRACE.md
PUML=refactor/tui-ui-architecture-target.puml
SVG=refactor/tui-ui-architecture-target.svg

for f in "$README" "$REFACTOR" "$STATUS" "$INVENTORY" "$FEATURES" "$ARCH" "$DECISIONS" "$TRACE" "$PUML"; do
  [[ -f $f ]] || { fail "missing: $f"; exit 1; }
done

# ── 1. puml validates ─────────────────────────────────────────────────────────
section "1. puml syntax"

if command -v plantuml >/dev/null 2>&1; then
  if plantuml -checkonly "$PUML" >/dev/null 2>&1; then
    ok "$PUML parses cleanly"
  else
    fail "$PUML has syntax errors (run: plantuml -checkonly $PUML)"
  fi
else
  warn "plantuml not installed — skipping syntax check"
fi

# ── 2. SVG age vs puml age ────────────────────────────────────────────────────
section "2. SVG freshness"

if [[ -f $SVG ]]; then
  if [[ $PUML -nt $SVG ]]; then
    warn "puml is newer than SVG — regenerate: plantuml -tsvg $PUML"
  else
    ok "SVG is up to date"
  fi
else
  warn "SVG missing — generate: plantuml -tsvg $PUML"
fi

# ── 3. Phase headers referenced in STATUS exist in REFACTOR ───────────────────
section "3. phase headers"

PHASES_IN_STATUS=$(grep -oE '\bP[0-9](\.[a-z])?\b' "$STATUS" | sort -u)
MISSING=0
for p in $PHASES_IN_STATUS; do
  # Strip the sub-phase to check the parent phase header.
  parent=${p%%.*}
  if ! grep -qE "^## $parent\b" "$REFACTOR"; then
    fail "STATUS mentions $p but REFACTOR has no '## $parent' header"
    MISSING=$((MISSING+1))
  fi
done
[[ $MISSING -eq 0 ]] && ok "all referenced phases ($(echo "$PHASES_IN_STATUS" | wc -w | tr -d ' ') unique) have headers in REFACTOR"

# ── 4. INVENTORY paths exist (or have a non-existent fate) ────────────────────
section "4. INVENTORY paths"

# Extract the first column from rows that look like file paths. Skip the
# "to be created" capabilities table and the Unclear table (their entries
# describe future or open files).
SECTION_START=$(grep -nE '^## `crates' "$INVENTORY" | head -1 | cut -d: -f1)
SECTION_END=$(grep -nE '^## To be created|^## Unclear|^## Maintenance' "$INVENTORY" | head -1 | cut -d: -f1)

if [[ -z $SECTION_START || -z $SECTION_END ]]; then
  warn "couldn't locate INVENTORY file-row range — skipping path check"
else
  MISSING_PATHS=0
  CHECKED=0
  # Each line of awk output: <crate-dir>\t<path>\t<fate>
  while IFS=$'\t' read -r crate path fate; do
    [[ -z $crate || -z $path ]] && continue
    [[ $fate == "deleted" ]] && continue
    stripped="${path%/}"
    full="$crate/$stripped"
    CHECKED=$((CHECKED+1))
    if [[ ! -e $full ]]; then
      fail "INVENTORY row points at missing path: $full (fate=$fate)"
      MISSING_PATHS=$((MISSING_PATHS+1))
    fi
  done < <(sed -n "${SECTION_START},${SECTION_END}p" "$INVENTORY" | awk '
    /^## `(crates\/[^\/]+\/src)/ {
      match($0, /crates\/[^\/]+\/src/)
      crate = substr($0, RSTART, RLENGTH)
      next
    }
    /^## / { crate = "" }
    crate != "" && /^\| `/ {
      n = split($0, c, "|")
      # c[1] is empty (leading |), c[2] is path column, c[5] is fate
      gsub(/^[ \t]+|[ \t]+$/, "", c[2])
      gsub(/^[ \t]+|[ \t]+$/, "", c[5])
      gsub(/`/, "", c[2])
      printf "%s\t%s\t%s\n", crate, c[2], c[5]
    }
  ')
  [[ $MISSING_PATHS -eq 0 ]] && ok "all $CHECKED INVENTORY paths resolve in the worktree"
fi

# ── 5. INVENTORY row counts vs filesystem ─────────────────────────────────────
section "5. INVENTORY coverage"

for crate in ui tui engine protocol; do
  dir="crates/$crate/src"
  [[ -d $dir ]] || continue
  fs_count=$(find "$dir" -name '*.rs' -not -path '*/target/*' | wc -l | tr -d ' ')
  # Count rows where the path doesn't have a slash beyond the leading
  # crate/file shape — rough heuristic. We just count non-empty data rows
  # under the crate's `## crates/<crate>/src` header.
  inv_count=$(awk -v hdr="^## \`crates/$crate/src" '
    $0 ~ hdr     { in_section=1; next }
    /^## /       { in_section=0 }
    in_section && /^\| `/ { count++ }
    END          { print count+0 }
  ' "$INVENTORY")
  if [[ $fs_count -ne $inv_count ]]; then
    warn "$crate: filesystem has $fs_count .rs files, INVENTORY lists $inv_count rows"
  else
    ok "$crate: $fs_count .rs files match INVENTORY"
  fi
done

# ── 6. STATUS "Last update" freshness ─────────────────────────────────────────
section "6. STATUS freshness"

LAST_LINE=$(grep -m1 -E '^\*\*Last update:\*\*' "$STATUS" || true)
if [[ -n $LAST_LINE ]]; then
  # Extract YYYY-MM-DD
  if [[ $LAST_LINE =~ ([0-9]{4}-[0-9]{2}-[0-9]{2}) ]]; then
    last_date="${BASH_REMATCH[1]}"
    if last_epoch=$(date -j -f "%Y-%m-%d" "$last_date" +%s 2>/dev/null) ||
       last_epoch=$(date -d "$last_date" +%s 2>/dev/null); then
      now_epoch=$(date +%s)
      age_days=$(( (now_epoch - last_epoch) / 86400 ))
      if [[ $age_days -gt 14 ]]; then
        warn "STATUS last updated $age_days days ago ($last_date) — refresh it"
      else
        ok "STATUS updated $age_days days ago ($last_date)"
      fi
    else
      warn "couldn't parse 'Last update' date: $last_date"
    fi
  else
    warn "STATUS '**Last update:**' line present but no YYYY-MM-DD found"
  fi
else
  warn "STATUS missing '**Last update:**' line"
fi

# ── 7. No leaked engine::permissions refs ────────────────────────────────────
section "7. engine policy-free check"

LEAKS=$(grep -nE 'engine::permissions|engine::Permissions' "$REFACTOR" "$ARCH" "$STATUS" 2>/dev/null || true)
if [[ -n $LEAKS ]]; then
  fail "leaked engine::permissions references:"
  echo "$LEAKS" | sed 's/^/    /'
else
  ok "no engine::permissions references in REFACTOR/ARCH/STATUS"
fi

# ── Summary ───────────────────────────────────────────────────────────────────
echo
printf '\033[1msummary\033[0m  %d passed · %d warnings · %d failed\n' \
  "$PASS" "$WARN" "$FAIL"

[[ $FAIL -gt 0 ]] && exit 1
exit 0
