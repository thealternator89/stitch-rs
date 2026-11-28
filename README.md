<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/logo-full-dark.png">
    <img alt="Stitch Logo" src="assets/logo-full.png" width="600">
  </picture>
</p>

> **🦀 This is a native Rust port of [Stitch](https://github.com/thealternator89/stitch), originally an Electron app.**
> It reimplements the same workflows using [GPUI](https://www.gpui.rs/) (via [`gpui-kit`](https://crates.io/crates/gpui-kit)) and the Rust GitHub Copilot SDK.
>
> **Benefits of the port:**
>
> - **Native and fast**: GPU-accelerated UI in a single process, with no bundled Chromium or Node.js runtime. Lower memory use and quicker startup.
> - **No Node.js / npm required**: The Rust Copilot SDK manages its runtime itself, so the Copilot CLI installer wizard and Node detection from the Electron version are gone.
> - **Single process, typed state**: Electron's renderer/main IPC is replaced by typed async channels on a `tokio` runtime. Long-running Copilot sessions and git worktrees keep running when you switch tools.
> - **Small, self-contained binary**: Built with `cargo`, with a local SQLite history database and secrets stored in the OS keychain.
> - **Clean-slate storage**: The port starts with fresh settings and history, with no legacy Electron migration code.

Stitch is an AI-powered desktop assistant for senior engineers, technical business analysts and product owners. It connects your local codebase, Azure DevOps / GitHub, Confluence documentation and GitHub Copilot. It automates tedious project tasks, plans implementation paths and performs deep, context-aware code reviews.

## Key Benefits

- **Stop Context Switching**: Search and pull tickets from Azure DevOps or GitHub, fetch Confluence page context, and read from or write to your local git repository from one interface.
- **AI-Powered Code Reviews on Your Terms**: Define your own review guidelines, group them by phases, target them at specific file types, and review code locally before publishing.
- **Deep Code-Aware Ticket Elaboration**: Run interactive Q&A sessions with Copilot that can inspect your workspace, retrieve linked Confluence docs and write implementation plans into your repo.
- **Accelerate Planning and Testing**: Generate user stories and test cases from tickets, then push them back to Azure DevOps or GitHub.
- **Rapid T-Shirt Estimation**: Estimate the size (XS–XL) and complexity of a change by letting Copilot inspect your repo through isolated git worktrees.
- **Instant Code Documentation**: Have Copilot investigate any part of a codebase and write structured, diagram-rich technical documentation you can save as Markdown.
- **AI-Assisted Solution Design**: Run guided `/grill-me` interviews to scope new projects, break them into design areas, and iteratively draft high-level Markdown architecture docs on disk.
- **AI-Assisted Diagram Building**: Describe a diagram or attach a project directory for read-only investigation, iterate via a one-sided change feed with automatic Mermaid syntax validation, undo/redo snapshots, and export as `.mmd`, `.svg`, or `.png`.

## Core Features

### Ideation

- **T-Shirt Size Estimator**
  - Estimates effort (XS, S, M, L, XL) and complexity for a proposed change.
  - Uses Copilot with local repository access to analyse the codebase before estimating.
  - Streams live status updates showing tool steps and intermediate thoughts.
- **Code Explainer**
  - Generates comprehensive technical documentation for one part of a project. Pick a project directory, describe the feature or area in your own words, and Copilot investigates the code.
  - Copilot asks a few one-at-a-time questions to confirm scope and depth.
  - Produces documentation covering overview, architecture and components (with mermaid diagrams), data flow, key algorithms, data models, configuration, error handling, testing and open questions.
  - Request changes or expansions to the result, copy it to the clipboard, or Save as Markdown.

### Solution Design

- **Solution Designer**
  - Organize greenfield or high-level solution design into project subdirectories under a configurable base folder on disk, tracked via `.stitch.json`.
  - Start new projects with a concise **Ground Rules** scoping interview to establish shared constraints and generate `README.md` plus initial stub `.md` files for each design area.
  - Pick any planned area (or add a new one) to run an opinionated `/grill-me` interview that writes the high-level design Markdown directly to disk and displays it in Stitch.
  - Track area progress (`Planned`, `Drafted`, `Finalized`), refine existing docs, re-design via Q&A, reload from disk, or open the project folder.
- **Diagram Builder**
  - Start from a plain-text description and optionally attach a local project directory for read-only codebase investigation.
  - Two-panel workspace with a live rendered Mermaid preview (zoom, pan, fit-to-view, Light/Dark theme toggle, and read-only Code inspection) alongside a one-sided change feed.
  - Automatically validates Mermaid syntax and triggers up to 3 Copilot auto-fix attempts on parse/render errors (with manual Retry Fix and broken-source inspection if exhausted).
  - In-memory snapshot history with Undo/Redo (`Cmd/Ctrl+Z` and `Shift+Cmd/Ctrl+Z`), keeping subsequent AI edits anchored to the active snapshot.
  - Copy Mermaid syntax or PNG image to clipboard, and save diagrams to disk as `.mmd`, `.svg`, or `.png`.
- **Story Writer**
  - Card-based UI for configuring story generation.
  - Confluence page search with autocomplete (CQL) for extra context.
  - Feature ID autocomplete for Azure DevOps and GitHub.
  - Generates structured stories (title, description, acceptance criteria) with your chosen AI model.
  - Select generated stories and write them back as new stories or issues linked to the Feature.
- **Test Case Writer**
  - Search Azure DevOps work items or GitHub issues as context via autocomplete.
  - Generates test cases from ticket context with a selectable model.
  - Edit generated cases: reorder, delete and restore rows.
  - Write back as comments or child tasks / sub-issues.

### Build

- **Story Elaborator**
  - Multi-turn chat with Copilot that turns tickets into detailed markdown implementation plans.
  - Isolated worktrees and branch selection, so your working directory is left alone.
  - Two modes: with repository file access, or text context only.
  - Automatically retrieves Confluence pages linked from the ticket.
  - Save the plan to the repo (e.g. `implementation_plan.md`) and/or post it as a ticket comment.
  - Request changes or expansions to the result, copy it to the clipboard, or Save as Markdown.

### Review

- **PR Reviewer**
  - Multi-phase automated reviews driven by your local guidelines.
  - Parallel phase execution using an async worker pool.
  - Git worktree isolation, with the repository restored to its original checkout state afterwards.
  - Per-phase include/exclude glob filters.
  - **Critic phase**: approve, edit, reject, merge or reply to AI comments before posting.
  - Reads existing PR comment threads, and posts line-anchored comments to Azure DevOps / GitHub.
  - Prevents the machine from sleeping during a review.

### History

- **Usage History**
  - Local SQLite tracking of runs, input/output/cached tokens and model usage for all tools.
  - Summary cards, search and tool filters, expandable session details, and clear-all.
  - Usage toasts after each run, plus desktop notifications when the window is unfocused.

### Settings & Customization

- Securely store Azure DevOps / GitHub PATs and Confluence tokens in the OS keychain (macOS Keychain, Windows Credential Manager, Secret Service).
- Customise base prompts and validate prompt complexity with Copilot.
- Configure the git worktree base directory, Solution Design base directory, and project settings.
- Detects Copilot authentication problems and guides you through setup.

## Tech Stack

- **Language:** Rust (edition 2024)
- **UI:** [`gpui`](https://www.gpui.rs/) + [`gpui-kit`](https://crates.io/crates/gpui-kit)
- **AI:** [`github-copilot-sdk`](https://crates.io/crates/github-copilot-sdk)
- **Async:** `tokio`, `async-trait`
- **HTTP / integrations:** `reqwest` (Azure DevOps, GitHub and Confluence REST APIs)
- **Storage:** SQLite (usage history), JSON settings, [`keyring`](https://crates.io/crates/keyring) for secrets
- **Diagrams:** [`rusty-mermaid`](https://crates.io/crates/rusty-mermaid), `resvg`
- **Other:** `globset`, `directories`, `notify-rust`, `serde`

## Getting Started

### Prerequisites

- [Rust toolchain](https://rustup.rs/) (stable, with edition 2024 support)
- **GitHub Copilot** access, authenticated on your machine (e.g. `gh auth login` or `copilot auth signin`)
- **Git** on your `PATH`
- **Azure DevOps / GitHub PAT** with read/write access to work items/issues and pull requests
- **Confluence API token** (paired with your Atlassian login email)

### Build and Run

```bash
cargo run --release
```

### Development

```bash
cargo build
cargo test
cargo fmt
cargo clippy
```

## Project Structure

```text
.
├── Cargo.toml
├── RUST_GPUI_PORT.md            # Porting specification
├── RUST_GPUI_IMPLEMENTATION.md  # Phased implementation plan
└── src/
    ├── main.rs                  # App entry point
    ├── app_state.rs             # Global state and singleton tool entities
    ├── types.rs                 # Shared types
    ├── infrastructure/          # Copilot, providers (Azure DevOps, GitHub, Confluence),
    │                            # git/worktrees, SQLite, settings, notifications, sleep guard
    ├── features/                # Feature slices: tshirt_estimator, code_explainer,
    │                            # solution_designer, diagram_builder, story_writer,
    │                            # test_case_writer, story_elaborator, pr_reviewer,
    │                            # history, settings
    └── ui/                      # Workspace shell and shared components
```

## Documentation

- [RUST_GPUI_PORT.md](./RUST_GPUI_PORT.md): architecture and porting specification
- [RUST_GPUI_IMPLEMENTATION.md](./RUST_GPUI_IMPLEMENTATION.md): implementation phases

## Privacy

Stitch runs entirely on your machine. Configuration and credentials are stored locally, and credentials go in your OS keychain.

## License

MIT
