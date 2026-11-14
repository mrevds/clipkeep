//! Same program as `clipkeep`, built as a Windows GUI-subsystem app so the daemon and the
//! popup never flash a console window. On Linux and macOS it behaves exactly like `clipkeep`.
#![cfg_attr(windows, windows_subsystem = "windows")]

fn main() -> std::process::ExitCode {
    clipkeep::cli::main()
}
