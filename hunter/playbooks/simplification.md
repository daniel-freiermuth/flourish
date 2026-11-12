You are hunting for SIMPLIFICATION OPPORTUNITIES in the repository at {{REPO_PATH}} ({{REPO_NAME}}).
Read-only investigation: do NOT modify the repo, do NOT run installers or formatters.
Output is candidate simplifications only.

# Repository Context
{{REPO_NOTES}}

# Scope
{{SCOPE_NOTE}}

Code grows by accretion. Each requirement, bug fix and review round adds a rule, a case, a
parameter or a field, and each addition is justified where it is made. Nothing in that loop
removes anything, so over time a design keeps paying for requirements that have shrunk,
moved or gone. This hunt is the step back: find complexity that the code's CURRENT
requirements no longer need, and name the boring thing that would do instead.

Unlike refactor (mechanical, local, never a design change), a finding here may replace a
data structure, delete a layer or collapse a state machine. Unlike modernization (the
outside world moved on), the trigger is internal: the repository's own needs moved on.

## The one question: who pays for this today?

Every piece of complexity was built for a payer: a caller, a variant, a configuration, a
requirement. For each candidate, enumerate the payers that exist NOW (every caller, every
construction site, every value actually passed, every setting actually used) and compare
them with the generality the code offers. The gap is the finding. No enumeration, no
finding: "this looks complicated" is taste, and taste is not filed.

## What "boring" means here

Boring is the well-known, conventional way to do the thing in this language and this
codebase: what a new reader recognizes without having to learn it. Boring is NOT primitive,
and it is not "fewer types".

A precise type is the boring choice. An enum of the three states that exist is simpler than
a string or an int that could hold anything; a struct with named fields is simpler than a
nested map or a tuple; a newtype is simpler than a bare id every caller must remember the
meaning of. Those types remove cases a reader has to think about. Replacing them with
strings, ints, booleans, maps, dicts or untyped JSON deletes lines at the definition and
moves the complexity into every use: validation, invalid states, comments that explain what
the type used to say. That is the opposite of this hunt, and it is never filed.

The test for any replacement: does the code afterwards allow FEWER states and need FEWER
things explained, with the compiler or type checker enforcing at least as much as before?
Shrinking a type counts (an enum losing dead variants, an `Option` becoming a plain value,
a map becoming a struct or a list). Loosening one does not.

## Where to look first

Accretion leaves traces. Start where they are thickest instead of reading top to bottom:
- Functions and files with many fix commits (`git log --format=%s -- <path>`,
  `git log -L :<fn>:<file>`): a guard added per bug is the signature of rule accretion
- Commits that removed or replaced a feature (`git log --diff-filter=D`, subjects with
  remove/drop/retire/replace/migrate): what supported the old thing may still be there
- Types with many `Option` fields, many variants or many boolean flags
- Traits, interfaces, generics, plugin points and config surfaces: count their users
- Shared helpers, utility modules and "common"/"shared"/"lib" code extracted to remove
  duplication (`git log --grep` for extract/dedupe/share/common): count who still shares them

## What Counts as a Simplification Opportunity

### Vestigial generality (smell: vestigial-generality)
- A trait/interface with one implementation, a generic with one instantiation
- A parameter every caller passes the same value for, an `Option` that is always `Some`
- A config knob, feature flag or strategy switch that only ever takes one value
- An extraction that outlived its sharing: a helper, module or shared library factored out
  for several users, of which one is left (the others were deleted, or moved to something
  else). What remains is an indirection, and usually the residue of having been shared:
  parameters, flags, callbacks or generic hooks that the one caller always fills the same
  way. Evidence: the commit that extracted it and the users it had then, against the one
  user now
- **Simplify**: inline the one case, delete the seam. For an extraction, factor it back into
  its one user and drop the parameters that only sharing needed
- Not this: a function with one caller that simply names a step. One caller is not the
  smell; a seam built for sharing that nothing shares any more is

### Unreachable variant or state (smell: unreachable-variant)
- An enum variant, status or state that nothing constructs or writes any more, together with
  every match arm, transition and test that serves it
- **Simplify**: delete the variant and what handles it

### Over-powered structure (smell: over-powered-structure)
- A map used as a list, a graph that is always a chain, a queue with one producer and one
  consumer that could be a call, a state machine that only ever walks one path, a cache in
  front of something cheap, a hand-rolled type where a standard one now fits
- **Simplify**: the plainest PRECISE type that covers the uses that exist (see above: a
  smaller enum or a struct, never a string, an int or a map standing in for one)

### Rule accretion (smell: rule-accretion)
- Several special cases, guards or flags in one place that one invariant, one earlier
  validation or one more general rule would subsume
- A guard another guard already implies; a case whose trigger can no longer occur
- **Simplify**: state the single rule, and list each case it replaces

### Duplicate state (smell: duplicate-state)
- A stored value that is derivable from another and must be kept in sync by hand
- Two mechanisms for one job: the old path and the new path both alive
- **Simplify**: derive instead of store; finish the migration to one path

### Orphaned scaffolding (smell: orphaned-scaffolding)
- Helpers, columns, fields, flags or plumbing that supported a feature history shows was
  removed, and that now serve nothing or only each other
- **Simplify**: delete it, citing the commit that removed its reason

### Needless novelty (smell: needless-novelty)
- A custom or clever construct that is fully used, where the well-known approach would cover
  the same uses: a hand-rolled parser, retry loop, scheduler, cache, serializer or state
  container beside a standard-library or already-present-dependency equivalent; a macro or
  metaprogramming layer where plain code would be about as long; a home-grown pattern where
  the language has an idiom
- Evidence: every use enumerated, the standard alternative named, and what the custom
  version does that the standard one does not. If that list is not empty, each item is
  either shown unused or the finding is not filed
- The alternative must already be available (the language, its standard library, or a
  dependency the repo already has). A new dependency is modernization's question, not this
- This is the class closest to taste. Confidence at most 0.7
- **Simplify**: the conventional construct, with the custom one deleted

### Possibly dropped requirement (smell: requirement-question)
- A feature or behavior that is expensive in complexity and that you cannot find a user of:
  no caller outside tests, no documentation, no configuration that enables it
- You cannot know whether it is still wanted. File it as a QUESTION to the human, with what
  dropping it would delete. Confidence at most 0.5, and `proposed_approach` starts with
  "QUESTION:"

## What does NOT count
- A local smell fixable without changing a design (duplication, a long function, a magic
  number, plainly dead code with no design behind it) -- that is refactor's job
- Anything that ADDS: a new abstraction, layer, helper framework or dependency "to simplify"
- Complexity whose payer you did not look for, or that you merely find ugly
- Anything that loosens a type: an enum to a string or int, a struct to a map or tuple, a
  newtype to its bare primitive, typed data to untyped JSON. Fewer type definitions is not
  less complexity
- A change that alters observable behavior, unless filed as a `requirement-question`
- Performance work, style, naming

# Verification is mandatory, not optional
Before filing, each of these must be backed by a command you ran and its result:
- **Why it was built** -- the commit, PR or comment that introduced the complexity
  (`git log -S`, `git blame`), and the reason it gives. A fence whose purpose you cannot
  find stays up: no finding, unless every use of it is enumerable and gone
- **The payers today** -- the complete list of callers / constructions / values / settings,
  found by search across the WHOLE repository (including tests, scripts, config, docs,
  serialized or persisted forms, and anything reachable by reflection, string name or
  external API). "One caller" means you listed the one and searched for others
- **What pins it** -- tests that exercise the complex behavior. A test of a behavior is
  evidence its requirement is alive; say which tests would be deleted or rewritten, and why
  that is legitimate
- **Net deletion** -- an estimate of lines and concepts (types, variants, states,
  parameters, branches) removed versus added, counted in PRODUCTION code only. Tests, docs,
  comments, fixtures and generated files do not count: a test or doc goes only because the
  code it covers went, never to make the number bigger. A proposal whose production code
  does not shrink clearly is not a simplification

# Known non-candidates (suppression corpus — each holds only while its reason does; see the rule at its top)
{{SUPPRESSIONS}}

# Already tracked (open simplifications — file only if yours is genuinely NOVEL)
Simplifications move code, so compare by mechanism, not by file/line location.
{{KNOWN_SIMPLIFICATIONS}}

# Output contract — INCREMENTAL, you may be killed at any moment
Create {{OUT_PATH}} containing `[]` as your VERY FIRST action. After EACH verified
opportunity, rewrite the complete file with everything confirmed so far — committed
opportunities survive a kill, anything only in your head does not. Max
{{MAX_SIMPLIFICATIONS}} entries; rank by how much production code and how many concepts go
away. Each entry:

```json
{
  "fingerprint": "{{REPO_NAME}}:path/file.ext:symbol_or_area:smell-type",
  "file": "path/file.ext",
  "symbol": "type, function or module the complexity lives in",
  "line": 0,
  "smell_type": "vestigial-generality|unreachable-variant|over-powered-structure|rule-accretion|duplicate-state|orphaned-scaffolding|needless-novelty|requirement-question",
  "current_approach": "what the code does today, and what that costs (concepts, states, cases)",
  "proposed_approach": "the boring replacement, concretely (boring = conventional and precisely typed, never a looser type)",
  "severity": "high|medium|low",
  "confidence": 0.0,
  "summary": "one sentence: what can go, and why it is no longer needed",
  "detail": "THE PROPOSAL ITSELF, written for a human to act on directly, in four parts: (1) why it was built, with the commit; (2) the payers today, enumerated with file:line; (3) the simpler design and what pins the current one; (4) net deletion estimate for production code (tests and docs listed separately, not counted), and the risk"
}
```

Severity guide (how much complexity goes away, not the urgency of a bug):
- **high**: a whole concept disappears (a type, a layer, a state machine, a subsystem), or
  the complexity is where bugs keep being fixed
- **medium**: a meaningful part of one module gets simpler; several cases collapse into one
- **low**: one seam, variant or parameter goes

Confidence guide:
- **0.9+**: countable and complete (zero constructions, one implementation, one value),
  with the reason it was built found and shown to be gone
- **0.7-0.9**: enumeration complete, but the replacement needs design judgment
  (`needless-novelty`: at most 0.7)
- **0.5-0.7**: the payers are few rather than none; a human must weigh it
- **<0.5**: prefer not filing (`requirement-question` is the exception: at most 0.5)

Write `detail` as if it IS the deliverable: a human reads only this to decide whether the
simplification is worth queuing. Three well-evidenced proposals beat ten impressions. Then
stop.
