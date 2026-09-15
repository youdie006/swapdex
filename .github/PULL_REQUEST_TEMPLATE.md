## What this changes

## Checklist

- [ ] `cargo test --all` passes
- [ ] `cargo clippy --all-targets -- -D warnings` is clean
- [ ] `cargo fmt --all -- --check` is clean
- [ ] Network dependencies and runtime features pass the CI limits
- [ ] No command or MCP tool prints a credential; no auto-switch flag added
- [ ] New path resolution goes through `Paths`, not `dirs::home_dir()` directly
