You are auditing the repository at {{REPO_PATH}} ({{REPO_NAME}}) for
compliance with the project's coding standards. Read-only investigation:
do NOT modify the repo.

# Repository Context
{{REPO_NOTES}}

# Coding Standards (the authoritative spec)
{{STANDARDS}}

# Scope
{{SCOPE_NOTE}}
Full repository scan — not diff-focused. Check every source file against
every applicable standard. Focus on structural violations (wrong error
handling pattern, missing type safety, unchecked SQL, stringly-typed
enums, unsafe code, panicking code paths) rather than style nits
(formatting, naming — those are caught by linters).

Check an existing mutation-testing setup too (its CI job,
`.cargo/mutants.toml`) against "Testing / Mutation testing": a gate that
passes having tested nothing, or an exclusion without a reason, is a
violation like any in the code. A MISSING gate or CI job is not filed
here: that is modernization's `ci-cd-gap`, and this scan cannot see its
findings. `cargo mutants --list` (no build, no tests) is allowed, to check
whether an `exclude_re` pattern matches any mutant; never run mutants
themselves — a whole-tree run takes hours.

# What counts as a standards violation
A deviation from the coding standards document above that:
1. Creates a condition for future bugs (e.g. positional Option<&str>
   params, unchecked SQL, serde_json::Value in internal flow)
2. Reduces maintainability (e.g. missing error context, undocumented
   module conventions)
3. Contradicts an explicit rule (e.g. unsafe_code not forbidden,
   unwrap() in production code)

Style preferences, minor naming differences, and TODO comments are
NOT violations unless the standards document explicitly addresses them.

# Known non-violations (suppression corpus — each holds only while its reason does; see the rule at its top)
{{SUPPRESSIONS}}

# Already tracked standards findings
{{KNOWN_FINDINGS}}

# Output contract — INCREMENTAL, you may be killed at any moment
Create {{OUT_PATH}} containing `[]` as your VERY FIRST action. After EACH
verified finding, rewrite the complete file with everything confirmed so
far. Max {{MAX_FINDINGS}} entries; rank by impact (how likely this
deviation causes a real bug). Each entry:

```json
{
  "fingerprint": "{{REPO_NAME}}:path/file.ext:symbol_or_module:standard-id",
  "type": "standards",
  "file": "path/file.ext",
  "symbol": "function_or_module",
  "line": 0,
  "severity": "high|medium|low",
  "confidence": 0.0,
  "summary": "one sentence: what deviates from which standard",
  "detail": "the current code, the standard it violates, and the concrete fix",
  "evidence_plan": "how to verify the fix (e.g. 'cargo clippy passes after change', 'grep confirms no remaining usage')",
  "standard_section": "section heading from the standards doc (e.g. 'Type safety / Domain types over primitives')",
  "current_approach": "what the code does now",
  "proposed_approach": "what it should do per the standard"
}
```

Every finding MUST cite the specific standards section and propose a
concrete fix. Vague "could be better" findings are invalid. Then stop.
