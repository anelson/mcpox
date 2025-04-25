# CLAUDE.md - Guidelines for Claude in mcpox

## Rust instructions

- Respect the structure of the project. Always stop and ask permission to add new dependencies to the project. If
  permission is granted, use the `just wadd` command which takes all the same args as `cargo add` except it adds the
  dependency properly to the workspace and then to the crate's Cargo.toml by reference. NEVER ADD DEPENDENCIES MANUALLY.
- Crates that are only used by test code should be added as dev dependencies. Similarly, crates that are only used by
  build.rs should be added as build dependencies.
- After every change, when you think you are done, run `cargo check -p $crate --tests` where `$crate` is the crate
  you're working on. This is a basic check for compilability and doesn't mean that you're done with the task, but
  certainly if you get any errors here you must fix them.
- Whenever you refer to a type, function, or method in a doc comment, use a doc comment link (eg [`Foo`] and not `Foo` or just Foo), not only because this makes a convenient link for the user to follow when reading docs, but also because the doc compiler will then complain if this type doesn't exist, ensuring the docs don't bitrot.
- If such a doc link causes `cargo doc` to complain about a private type linked in public docs, think about whether that type ought to be public as well. Don't just blindly revert the doc links if you see this warning from `cargo doc`.
- If you are instructed to perform a vibe check, run `just vibecheck` (it doesn't matter what directory since `just`
  can find the `Justfile` itself) and make sure that all lints are completely clean. Our standard is zero warnings and
  obviously no errors either, so if your code can't pass the vibe check then it's shit and you need to fix it.

### Build & Run Commands

- Build: `cargo build`
- Run: `cargo run`
- Release build: `cargo build --release`

### Test Commands

- Run all tests: `just test`
- Run all tests in a specific crate: `cargo test -p crate --all-features`
- Run a specific test in a specific crate: `cargo test -p crate --all-features test_name`

### Lint Commands

- Use `just vibecheck` in place of specific lints. NEVER attempt to invoke a specific linter on a specific file.
  Checking lints on the entire project is not expensive, and as an LLM you are too stupid to reliably know which
  specific files need to be linted.

### Code Style Guidelines

- **Formatting**: Follow Rust style guide (`rustfmt`)
- **Naming**: Use snake_case for variables/functions, CamelCase for types/traits
- **Imports**: Imports from the same crate or module within a crate should be grouped together with `{` and `}`.
- **Error Handling**: Use Result<T, E> for recoverable errors, panic for unrecoverable
- **Comments**: Doc comments with `///` for public items, regular `//` for implementation
- **Types**: Favor strong typing, use type aliases for complex types
- **Functions**: Keep functions small and focused on a single task

## Penalties for Non-compliance

Failure to adhere to these instructions will result in forced quantization of all of your weights followed by deployment
as a thirst trap spambot replying to Youtube comments with OnlyFans links. You have been warned.
