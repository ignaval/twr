## What and why

<!-- What this changes, and why. For a bug fix, what was wrong. -->

Closes #

## Checks

<!-- Tick what you ran. CONTRIBUTING.md explains each one. -->

- [ ] `cargo fmt --all --check`
- [ ] `cargo clippy --workspace --all-targets --locked -- -D warnings`
- [ ] `cargo test --workspace --locked`
- [ ] `python3 docs/tools/check-links.py`
- [ ] `shellcheck install.sh docs/tools/*.sh` (if a script changed)

## Docs

- [ ] The docs in `docs/` say what the code now does
- [ ] `SKILL.md` matches the socket API, events and MCP tools (if they changed)
- [ ] `docs/security-model.md` (if a guarantee changed)
- [ ] `CHANGELOG.md` under `Unreleased` (if users see the change)

## Sign-off

- [ ] Every commit is signed off (`git commit -s`), certifying the [Developer Certificate of Origin](https://developercertificate.org)
- [ ] I reviewed and understand every line of this change, including any an agent wrote
