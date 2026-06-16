# ADR-0026: Web syntax highlighting for the rules and flow editors

- **Status:** Accepted
- **Date:** 2026-06-15
- **Deciders:** Epiphany maintainers
- **Phase:** Post-roadmap (web UI, W3 authoring)

## Context

The rules and flow editors are plain `<textarea>`s with debounced server-side
validation (`previewRules` / `previewFlow` return a message plus a 1-based
line/column on failure). They have no syntax highlighting; ADR-0020 named
highlighting the one acknowledged-missing editor affordance and sanctioned a
lazy-loaded, expert-only Monaco as an option without committing to it.

W3 (flow + rules authoring polish) needs to decide how to add highlighting and
richer authoring before building the editor increments, so they do not bake in a
choice that later gets reversed.

The project is deliberately dependency-light: Radix headless primitives plus
vendored components, no Tailwind, no component library, and an in-house
TypeScript stripper rather than swc. The rules and flow surfaces both already
have hand-written lexers/parsers in Rust (epiphany-calc, the flow type stripper).

## Decision

**Add highlighting in-house: a small, dependency-free tokenizer rendered as an
overlay behind the existing textarea. Do not adopt Monaco or a third-party
highlighter.**

Shape:

- A reusable `CodeEditor` component layers a syntax-highlighted, `aria-hidden`
  `<pre>` directly under a transparent-text `<textarea>` (the standard overlay
  technique). The textarea stays the source of truth and the accessible control;
  the overlay only colors tokens. Both use the same monospace font and metrics so
  glyphs align.
- Highlighting is a per-language regex/scan tokenizer (keywords, strings,
  numbers, comments, punctuation) of well under a few hundred lines, reusing the
  token vocabulary the Rust lexers already define. No grammar engine.
- Inline error marking reuses the line/column the validation API already returns:
  the editor underlines/gutter-marks the reported line and shows the message.

## Alternatives considered

- **Monaco (lazy-loaded, expert-only).** Rejected. Monaco brings roughly 800 KB
  gzipped of VS Code libraries and a worker/loader setup. Even behind a dynamic
  import it is a large, opaque dependency surface for a feature only expert users
  touch, and it contradicts the hand-written-lexer, minimal-dependency ethos. The
  lazy-load harness it would need is itself avoided by going in-house.
- **A light third-party highlighter (Prism ~15 KB, CodeJar ~5 KB).** Smaller than
  Monaco and tempting, but still a new runtime dependency (and a new license to
  clear through cargo-deny's spirit on the web side) for something a ~200-line
  in-house tokenizer covers, with full control over the rules/flow token sets.
  Rejected in favor of zero new dependencies.
- **Stay plain textarea, no highlighting.** Rejected: highlighting and inline
  error marking are the core of the W3 authoring polish and materially help the
  expert path; the in-house cost is small.

## Consequences

- W3 builds a vendored `CodeEditor` (overlay + tokenizer + error marking) used by
  both the rules and flow workspaces. No npm dependency is added; the main bundle
  stays within the ADR-0020 budget.
- Highlighting is best-effort and cosmetic: a tokenizer bug can only mis-color,
  never block editing or change what is saved (the textarea value is canonical).
- The token sets live next to the editors and are kept loosely in step with the
  Rust lexers; they do not need to be a perfect parser, only a good colorizer.
- If a future need genuinely demands a full language service (completion,
  go-to-definition), that is a separate decision with its own ADR; this records
  that highlighting alone does not justify a heavy editor dependency.
