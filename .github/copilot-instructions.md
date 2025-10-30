# Copilot Instructions for Kimojio Repository

## Code Quality and Formatting

When making code changes to this Rust project, always follow these steps:

### Required Steps for All Changes
1. **Format code**: Run `cargo fmt` to ensure consistent code formatting
2. **Run clippy**: Run `cargo clippy` to check for common mistakes and improve code quality
3. **Run clippy with all features**: Run `cargo clippy --all-targets --all-features` to ensure comprehensive linting

### Pre-commit Checklist
- [ ] `cargo fmt` has been run and any formatting changes committed
- [ ] `cargo clippy` passes without warnings
- [ ] `cargo clippy --all-targets --all-features` passes without warnings
- [ ] Tests still pass after changes
- [ ] Changes are minimal and focused on the specific issue being addressed

### Notes
- Always run these tools before committing code changes
- Address any clippy warnings that are introduced by your changes
- Ensure formatting is consistent with the project's style guidelines