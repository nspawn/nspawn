# Contributing

## Before sending a change

- `cargo fmt`, `cargo clippy --all-targets -- -D warnings` and `cargo test`
  pass.
- New behaviour has a unit test. Changes to machines, networking or registry
  access also get a step in `tests/e2e.sh`, and the suite has been run on a VM
  (see docs/HACKING.md).
- README.md, docs/USAGE.md and docs/COMPOSE.md describe what users see,
  docs/DBUS.md the bus interface, and docs/ARCHITECTURE.md how it works. Update
  whichever applies.

## Commit messages

- Subject in the imperative, at most 72 characters, no trailing period,
  naming the change itself: `Reject image references in start`, not
  `Address review comments`.
- Body: a line of why when the subject does not say it, then one bullet per
  change when the commit carries several. A claim about tests names the
  tests. Wrap at 72 columns.
- One logical change per commit. Fixups are squashed before merging.
- No trailers, signatures or tool names.

## License

nspawn is offered under the MIT license or the Apache License 2.0, at the
user's option (LICENSE-MIT, LICENSE-APACHE). By sending a change you offer it
under both, without further terms, so that it can be included as it is.

## Style

- Plain ASCII in code comments, documentation and messages. No emojis.
- Error messages say what failed and what to do next, naming the path, unit
  or machine.
- Comments explain why, not what.
