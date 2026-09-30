//! `tts` binary: alias entry point over the same library as `yandex-tts`
//! (supports `tts skill_install <PATH>`).

fn main() -> std::process::ExitCode {
    yandex_tts::main()
}
