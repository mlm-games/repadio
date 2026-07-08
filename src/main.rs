#[cfg(not(any(target_os = "android", target_arch = "wasm32")))]
fn main() -> anyhow::Result<()> {
    repadio::run_desktop()
}

#[cfg(any(target_os = "android", target_arch = "wasm32"))]
fn main() {}
