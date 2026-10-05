#![deny(unsafe_op_in_unsafe_fn)]

fn main() -> std::io::Result<()> {
    fog::cli::run()
}
