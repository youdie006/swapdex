# Codex profiles must retain managed account routing

## Problem and evidence

A supervised `codex --strict-config exec ... -p worker` job reaches the
installed Swapdex shim, but the shim treats every profile flag as an explicit
request to bypass managed routing. The native process consequently retains its
launch home's ChatGPT login even when another account is selected for serving.
Ordinary interactive sessions continue through the proxy, making inspection of
only those sessions insufficient to identify the independent jobs.

The observed executable arguments lacked `openai_base_url`, its environment
contained the old launch home, and it connected directly to the vendor. No
private job-state files, prompts, or credentials are required to reproduce the
launcher defect. A generated shim and a stub native executable reproduce it.

## Required behavior

- `-p worker`, `-pworker`, `--profile worker`, and `--profile=worker` retain
  managed routing for the built-in OpenAI provider.
- Profile names remain option values, including names such as `login`.
- Keep the complete argument vector and the existing `CODEX_HOME` unchanged
  except for the managed built-in provider URL override.
- Explicit command-line provider configuration, remote/local provider flags, authentication
  commands, and intentional proxy passthrough retain their existing behavior.
- A profile selecting a distinct custom provider continues to use that provider.

Codex `rust-v0.154.0` selects `<CODEX_HOME>/<name>.config.toml`, a full config
layer. Swapdex's override configures only the built-in OpenAI provider, matching
the existing behavior of ordinary launches: a profile alone does not opt out
of managed OpenAI routing, including a profile file's `openai_base_url`.
Explicit command-line provider overrides still opt out; profiles selecting a
distinct custom provider retain that provider. A stock-client fixture must
verify this compatibility rather than relying only on argument inspection.

## Change and validation

Consume separated profile values without setting `sx_plain`. Accept attached
profile forms without setting it either. Keep other bypass cases separate.

Add executable shim regressions for all four forms, option-value parsing, and
explicit provider exceptions. Add a stock-Codex loopback fixture with distinct
launch and selected account credentials. Assert the selected account reaches
the fake upstream and the launch account is not charged by a direct request.
Also verify a custom provider selected by profile remains authoritative.

Run the project gate and execute the stock-client fixture on the installed WSL
and M3 versions. Publish a patch release with installation and service records.
Updating a launcher affects new launches; already-running direct native jobs
are reported separately and are not silently cancelled or replayed.
