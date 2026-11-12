#![deny(unsafe_op_in_unsafe_fn)]

fn main() -> std::io::Result<()> {
    // Before `cli::run` because the TUI takes the terminal over there: tracing
    // is opt-in (`RUST_LOG`) and writes to the instance log dir, never to the
    // screen. An unset `RUST_LOG` installs nothing at all.
    fog::observability::init();
    fog::cli::run()
}
