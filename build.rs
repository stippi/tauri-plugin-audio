const COMMANDS: &[&str] = &[
    "init_session",
    "teardown_session",
    "enqueue_audio",
    "get_status",
];

fn main() {
    tauri_plugin::Builder::new(COMMANDS)
        .android_path("android")
        .ios_path("ios")
        .build();
}
