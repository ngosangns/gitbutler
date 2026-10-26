// CHANNEL selects GitButler's data directory inside but-path (option_env!).
// Cargo applies apps/gb-gpui/.cargo/config.toml only when the build's
// working directory is that folder. A build from the repository root leaves
// CHANNEL unset, and but-path then uses com.gitbutler.app.dev.
fn main() {
    println!("cargo:rerun-if-env-changed=CHANNEL");
    println!("cargo:rerun-if-env-changed=GB_GPUI_ALLOW_DEV_CHANNEL");

    let channel = std::env::var("CHANNEL").unwrap_or_default();
    let allow_dev = std::env::var("GB_GPUI_ALLOW_DEV_CHANNEL").ok().as_deref() == Some("1");
    if channel != "release" && !allow_dev {
        panic!(
            "gb-bridge: CHANNEL is {channel:?}, not \"release\". \
             but-path would use GitButler's dev data directory (com.gitbutler.app.dev) \
             and miss the installed app's project list. \
             Build from apps/gb-gpui so .cargo/config.toml applies \
             (`cargo build` inside apps/gb-gpui). \
             Set GB_GPUI_ALLOW_DEV_CHANNEL=1 to build another channel on purpose."
        );
    }
}
