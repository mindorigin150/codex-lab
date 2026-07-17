# Codex Lab Fork Feature Matrix

This file records behavior that Codex Lab adds to, strengthens beyond, or deliberately keeps
different from upstream Codex. It is a semantic checklist: matching file names or conflict-free
merges do not establish feature parity.

Last upstream comparison:

- Upstream: `openai/codex@5a85351dfe04ae2930858e54ecccba9e239b7ccd`
- Fork before integration: `mindorigin150/codex-lab@5baa3763c471f327f04144613065e6b9aa4a701b`
- Reviewed: 2026-07-17

## Fork Guarantees

- [x] **Context-isolated explorers.** Multi-agent V2 explorers default to a fresh thread when
  `fork_turns` is omitted or set to `none`, and reject `all` or a positive turn count. Upstream
  defaults omitted `fork_turns` to `all` for every role.
  - Implementation: `codex-rs/core/src/tools/handlers/multi_agents_v2/spawn.rs`
  - Tests: `multi_agent_v2_explorer_defaults_to_fresh_context` and
    `multi_agent_v2_explorer_rejects_inherited_context`
- [x] **Role sandboxing bounded by the parent.** Built-in explorer and reviewer roles are
  read-only. Their effective runtime permission profile is intersected with the parent's profile,
  and the parent network proxy remains the capability ceiling.
  - Implementation: `codex-rs/core/src/agent/builtins/` and
    `codex-rs/core/src/tools/handlers/multi_agents_common.rs`
- [x] **Leaf delegation enforcement.** Child tool exposure follows the configured depth limit,
  with runtime enforcement and regression coverage in addition to schema-level hiding.
- [x] **Blocking completion barriers.** Explorer and reviewer work blocks parent completion until
  the matching result is delivered. New user input can wake the parent without cancelling the
  child.
  - Implementation: `codex-rs/core/src/agent/control/barrier.rs` and
    `codex-rs/core/src/session/turn/blocking_agent_barrier.rs`
- [x] **Generation-safe delegation lifecycle.** Interrupt, retry, cancellation, late completion,
  and resumed-session paths keep completion receipts associated with the correct child task.
- [x] **Live task visibility.** `list_agents` reports each agent's most recent plaintext user or
  inter-agent instruction so the parent can avoid duplicate assignments; encrypted instructions
  deliberately clear this field.
  - Implementation: `codex-rs/core/src/agent/control.rs` and
    `codex-rs/core/src/agent/registry.rs`
- [x] **`agents` tool namespace.** Codex Lab avoids the reserved `collaboration` schema while
  retaining `[agents]` as the configuration namespace.
- [x] **Bounded unified-exec output artifacts.** Large command output is retained in private local
  artifacts instead of unbounded model context, with explicit output metadata and lifecycle
  cleanup.
  - Implementation: `codex-rs/core/src/unified_exec/output_artifact.rs`
- [x] **Encrypted tool boundary enforcement.** Encrypted inputs remain restricted to tool schemas
  and execution paths that explicitly opt in.
- [x] **Terminal LaTeX images.** TUI math is parsed with MathJax, rendered as transparent PNG, and
  displayed through terminal image placement with resize, pager, backtrack, and scrollback
  cleanup support.
  - Implementation: `codex-rs/tui/src/formula_parser.rs`, `formula_render.rs`, and
    `formula_runtime.rs`
- [x] **Portable `codex-lab` installation.** Versioned releases, isolated configuration, doctor
  checks, and a bundled Bubblewrap path work without replacing the stock `codex` command or
  requiring `sudo`.
  - Implementation: `scripts/install/install-codex-lab.sh`

## Upstream Capabilities Integrated by This Fork

- [x] Generic `fork_turns=none/all/N` history selection and paginated-history replay.
- [x] `[agents]` enablement, concurrency settings, compatibility aliases, and config-lock state.
- [x] Default subagent model and reasoning settings, explicit overrides, and backend-compatible
  model filtering.
- [x] Session/IO separation, V2 metadata restoration, and external-agent migration.
- [x] Basic completion notifications, interrupt support, and depth-based tool exposure.

## Deliberate Combined Semantics

- [x] Explorer always uses fresh context and cannot request full-history model overrides.
- [x] Non-explorer V2 agents may combine `fork_turns=all`, an explicit `agent_type`, and validated
  model or reasoning overrides.
- [x] Role configuration is applied without dropping runtime workspace roots, provider state,
  reasoning summary, or the parent's permission ceiling.
- [x] Upstream TUI reflow/session changes must retain formula image cleanup and placement rebuilds.

## Upstream Sync Checklist

- [ ] Update the upstream commit and review date above.
- [ ] Compare behavior and tests, not only commit subjects or merge conflicts.
- [ ] Recheck explorer fork defaults, role sandboxing, and parent permission intersection.
- [ ] Recheck completion barriers, generation receipts, interrupt/retry, and resume behavior.
- [ ] Recheck `agent_type`, full-history, default-model, and backend-filtering semantics together.
- [ ] Recheck TUI formula rendering across resize, pager, backtrack, and history replay.
- [ ] Mark a fork guarantee as upstream-equivalent only after an upstream test exercises the same
  contract; then remove duplicate code in a separate reviewed change.
