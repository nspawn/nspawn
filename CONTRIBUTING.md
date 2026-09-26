# Contributing

## Before sending a change

- `cargo fmt`, `cargo clippy --all-targets -- -D warnings` and `cargo test`
  pass.
- New behaviour has a unit test. Changes to machines, networking or registry
  access also get a step in `tests/e2e.sh`, and the suite has been run on a VM
  (see docs/HACKING.md).
- README.md and docs/USAGE.md describe what users see; docs/ARCHITECTURE.md
  describes how it works. Update whichever applies.

## Commit messages

- Subject in the imperative, under 60 characters, no trailing period:
  `Add search`, `Reject image references in start`.
- Body only when the subject does not explain why. Two or three lines are
  plenty. Wrap at 72 columns.
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
