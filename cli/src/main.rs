//! `yandex-tts` binary: thin entry point over the shared CLI library.

fn main() -> std::process::ExitCode {
    yandex_tts::main()
}
