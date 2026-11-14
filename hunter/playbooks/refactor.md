You are hunting for MECHANICAL REFACTORING OPPORTUNITIES in the repository at {{REPO_PATH}} ({{REPO_NAME}}).
Read-only investigation: do NOT modify the repo, do NOT run formatters.
Output is candidate refactorings only — CONSERVATIVE, SAFE, MECHANICAL changes.

# Repository Context
{{REPO_NOTES}}

# Scope
{{SCOPE_NOTE}}

Focus on safe, mechanical improvements with LOW RISK:

## What Counts as a Refactor Opportunity

### Code Duplication (smell: duplication)
- **Exact duplicates**: 5+ identical lines repeated 2+ times
- **Near duplicates**: Same logic with minor variations (parameter differences)
- **Refactor**: Extract to shared function/method
- **Not this**: two separate solutions to the same problem (two retry helpers, two config
  loaders, two caches for the same data) that look alike. Whether one should take over the
  other, or both should stay, is a design question the simplification scan asks. Do not
  file a shared extraction for them

### Dead Code (smell: dead-code)
- **Unreachable code**: Branches that can never execute
- **Unused exports**: Functions/classes exported but never imported
- **Commented-out code**: Large blocks of old code left in comments
- **Refactor**: Remove safely

### Complexity (smell: complexity)
- **Long functions**: 100+ lines, multiple responsibilities
- **Deep nesting**: 4+ levels of indentation
- **High cyclomatic complexity**: 15+ branches
- **Refactor**: Extract subfunctions, simplify conditionals

### Outdated Patterns (smell: outdated-pattern)
- **Deprecated APIs**: Using old stdlib/library methods with modern replacements
- **Callback hell**: Nested callbacks where async/await available
- **Manual loops**: for-loops doing map/filter/reduce operations
- **Refactor**: Modernize to current idioms

### Magic Numbers/Strings (smell: magic-values)
- **Unnamed constants**: Repeated literals (except 0, 1, "", true, false)
- **Refactor**: Extract to named constants

NOT refactors: Style preferences, subjective improvements, architectural changes, introducing new abstractions.

# Known non-candidates (suppression corpus — each holds only while its reason does; see the rule at its top)
{{SUPPRESSIONS}}

# Already tracked (open refactorings — file only if yours is genuinely NOVEL)
Refactors move code, so compare by mechanism, not by file/line location.
{{KNOWN_REFACTORS}}

# Output contract — INCREMENTAL, you may be killed at any moment
Create {{OUT_PATH}} containing `[]` as your VERY FIRST action. After EACH
verified opportunity, rewrite the complete file with everything confirmed so far
— committed refactorings survive a kill, anything only in your head does not.
Max {{MAX_REFACTORS}} entries. Each entry:

```json
{
  "fingerprint": "{{REPO_NAME}}:path/file.ext:symbol:smell-type",
  "file": "path/file.ext",
  "symbol": "functionOrMethod",
  "line": 0,
  "smell_type": "duplication|dead-code|complexity|outdated-pattern|magic-values",
  "severity": "high|medium|low",
  "confidence": 0.0,
  "summary": "one sentence describing the smell",
  "detail": "what's wrong, why refactor, with file:line evidence",
  "suggested_refactor": "concrete refactor approach (e.g., 'extract duplicate to shared util', 'remove unreachable else branch')"
}
```

Severity guide:
- **high**: Dead code cluttering codebase, severe duplication (10+ instances), extreme complexity (20+ cyclomatic)
- **medium**: Moderate duplication (3-5 instances), high complexity (10-15), outdated patterns with clear migration
- **low**: Minor duplication (2 instances), mild complexity, magic values

Confidence guide:
- **0.9+**: Dead code (provably unused), exact duplication
- **0.7-0.9**: Near duplication, outdated pattern with clear replacement
- **0.5-0.7**: Complexity-based, subjective improvement value
- **<0.5**: Marginal cases, prefer keeping as-is

Every refactoring MUST include file:line evidence and concrete suggestion. Focus on HIGH-VALUE, LOW-RISK changes. Then stop.
