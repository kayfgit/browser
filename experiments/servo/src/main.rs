//! Separate qualification harness; this does not register a browser provider.
#[cfg(windows)]
surfman::declare_surfman!();

#[cfg(windows)]
mod windows;

#[cfg(windows)]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    windows::run()
}

#[cfg(not(windows))]
fn main() {
    eprintln!("The Servo/WebView2 child-window experiment requires Windows.");
    std::process::exit(1);
}
