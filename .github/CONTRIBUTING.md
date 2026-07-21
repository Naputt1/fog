# Contributing to fog

Thank you for considering contributing to fog!

## Development Setup

1. Clone the repo: `git clone https://github.com/Naputt1/fog.git`
2. Build: `cargo build`
3. Run tests: `cargo test`
4. Check lint: `cargo clippy`
5. Format: `cargo fmt`

## Code Style

- Follow existing patterns in the codebase
- Keep functions focused and small
- Add doc comments to all public items
- No `unwrap()` in production code — use `expect()` with context or `?`
- Ensure `cargo clippy` is clean before submitting

## Pull Request Process

1. Fork the repository
2. Create a feature branch
3. Make your changes
4. Run `cargo test && cargo clippy && cargo fmt --check`
5. Submit a pull request

## Testing

- Add tests for new functionality
- Run `cargo test` to verify nothing is broken
- Integration tests are in `tests/`
