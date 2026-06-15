# ADR-0020: Web UI design system and information architecture

- **Status:** Accepted (model locked; realized across the W0-W5 increments)
- **Date:** 2026-06-15
- **Deciders:** Epiphany maintainers
- **Phase:** Post-roadmap web UI overhaul

## Context

The web client is functional but basic: plain React with about 700 lines of
hand-written CSS, a few color variables, no component primitives, no dark mode,
and an information architecture (a cube list plus four per-cube tabs) that does
not surface most of what the engine can do. The goal is an **ultra-polished,
ultra-friendly** UI that lets users manage the whole OLAP model, authors flows
and connectors simply, and spells everything out, guided by a UI/UX best-practice
review.

Two forces constrain the answer:

- **The dependency-conscious ethos is real and load-bearing.** The engine chose
  hand-written lexers, boa over V8, and dependency-free tests on purpose; the web
  client is deliberately zero-runtime-dependency. Any UI dependency must clear the
  same bar the backend does.
- **The KISS-for-the-end-user north-star.** A casual business/data-entry user
  must never be forced to see modeler or admin machinery, write MDX/rules/flows,
  or touch Git; power lives in the engine and surfaces stay shallow.

## Decision

**1. Design system: Radix primitives + vendored (shadcn-style) components + a
CSS-variable token system. No Tailwind, no component library.**

- **Radix UI primitives (headless) are adopted** for the interactive widgets that
  are genuinely hard to make accessible by hand: Dialog, Popover, Select, Tabs,
  Tooltip, DropdownMenu, Checkbox, and similar. They are headless (styled entirely
  by us), tiny, and add only the `@radix-ui/react-*` packages actually used. This
  is the rare dependency that reduces risk: the alternative is shipping
  inaccessible dialogs and selects. Accessibility target is WCAG 2.1 AA.
- **Components are vendored, not installed as a library.** Component source lives
  in `web/src/ui/`, owned and editable (the frontend equivalent of the
  hand-written lexers), styled with our tokens. No component-library dependency
  (MUI/Ant/Mantine) and no shadcn CLI.
- **Tokens are native CSS custom properties** in three layers - base (raw spacing,
  type, and color scales), semantic (intent names: surface, text-muted, success,
  danger, the sandbox-overlaid and element-denied and input-vs-calc cell colors),
  and component - with **dark mode** as a `:root[data-theme="dark"]` override of
  the semantic layer, toggled by a data attribute (no re-render). No Tailwind (a
  build-pipeline and authoring-paradigm dependency whose real value, tokens and
  runtime theming, we get from CSS variables for free) and no token-sync tooling.

**2. Two sanctioned heavier dependencies, both gated.** The **pivot grid** (the
defining surface, per the north-star and performance mandate) may take one
purpose-built grid dependency or be extended with a virtualization helper.
**Monaco** is lazy-loaded **only** in the expert flow/rule editors, never in the
app shell or the business-user paths, so the shell bundle stays lean. A CI bundle
ceiling guards the shell.

**3. Information architecture: a persona-gated shell.** The user's persona is
resolved from the existing four-level security lattice (None/Read/Write/Admin
plus model rights) and selects the shell:

- **Business user** lands directly in a Views / data-entry workspace; the sidebar
  shows only their Views, Subsets, and the Sandbox switcher. No Rules, Flows, or
  Dimensions chrome.
- **Modeler** gets the full sidebar with collapsible semantic groups: Data Models
  (Cubes, Dimensions, Hierarchies, Attributes), Calculation (Rules, Feeders),
  Automation (Flows, Flow Tests, Jobs/Schedules, Connections), Analysis (Subsets,
  Views, Sandboxes).
- **Admin** gets the modeler shell plus Admin (Security: Users/Groups, Object
  access, Element access, Cube grants; Audit; system health).

The sidebar is a direct projection of objects the engine already has - no invented
concepts. Supporting patterns: a **Cmd/Ctrl-K command palette** (fuzzy search over
objects and verbs - the expert fast-path that keeps the visible UI shallow), a
**right-panel inspector** (select an object, edit its properties/children/actions
without modal stacks), **breadcrumbs**, **tabbed content** with dirty-state guards,
and **status badges wired to real engine signals** (connection health, job
last-run/next-due, sandbox uncommitted count, feeder under-feed=error/
over-feed=warning, flow-test pass/fail).

**4. "Everything spelled out."** Plain-language labels with engine jargon hidden
behind progressive disclosure ("Some calculated values may be missing data" with
the term "under-feed" on expand); just-in-time contextual tooltips; teaching empty
states with one-click sample actions; a value-ordered setup checklist; consistent
object iconography and color; and one-click **provenance** ("why is this number
what it is?") via the existing calc explain tree as the core trust mechanism.

**5. Two dependent workstreams, each with its own ADR.** Managing the whole model
needs a **model-editing REST API** (create cube, add/edit dimensions, elements,
hierarchies, attributes) that does not exist yet; it is built underneath the Data
Models UI section as its own increment and ADR. The **Excel add-in** (ADR-0021) is
a thin REST client that reuses this web UI inside a WebView2 task pane.

## Alternatives considered

- **Tailwind CSS (+ shadcn CLI).** Rejected: a build-pipeline and authoring
  dependency for a team that chose plain CSS on purpose; its real value (tokens,
  runtime theming) is available from CSS variables with no dependency.
- **A component library (MUI / Ant Design / Mantine).** Rejected: large bundles,
  opinionated aesthetics to override, and CSS-specificity friction - against the
  whole ethos. Headless Radix plus our own styling gives polish without the lock-in.
- **Hand-rolling accessible dialogs/selects/menus.** Rejected: correct focus
  management, keyboard interaction, and ARIA are subtle and high-risk to maintain;
  Radix is the one dependency that lowers total risk.
- **Staying fully custom (no Radix).** Rejected for the interactive widgets only;
  retained everywhere else.

## Consequences

- New web dependencies limited to a handful of `@radix-ui/react-*` packages, plus
  (gated) one grid dependency and lazy Monaco; a CI bundle ceiling enforces the
  budget. Tokens and the vendored component contract are documented in `AGENTS.md`.
- A token migration of the existing CSS, a vendored primitive set, dark mode, and
  a persona-gated shell with a command palette and inspector.
- Accessibility becomes a gate (WCAG 2.1 AA), checked with an automated a11y pass.
- The model-editing REST API and the Excel add-in are tracked as their own
  ADR-backed increments; the UI for the Data Models section depends on the former.
- Realized incrementally (W0 foundation through W5 onboarding) on the project's
  build-review-release cadence, so each step ships green.
