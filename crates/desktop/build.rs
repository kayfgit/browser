//! Embed a Windows application manifest declaring PerMonitorV2 DPI awareness.
//!
//! Without a DPI-awareness manifest, Windows treats a GUI exe as DPI-unaware and
//! applies a compatibility shim (`__COMPAT_LAYER=HIGHDPIAWARE`). The WebView2
//! runtime (117+) detects that shim and, to dodge a DPI-related launch bug,
//! launches its engine processes via `explorer.exe` instead of parenting them to
//! us — which makes them float off as a separate "WebView2 Manager" group in Task
//! Manager. Declaring PerMonitorV2 here suppresses the shim, so WebView2 parents
//! its whole process tree under the shell and Task Manager nests it accordingly.

fn main() {
    #[cfg(windows)]
    {
        use embed_manifest::manifest::DpiAwareness;
        use embed_manifest::{embed_manifest, new_manifest};

        embed_manifest(
            new_manifest("kayf.browser.Shell").dpi_awareness(DpiAwareness::PerMonitorV2),
        )
        .expect("embed application manifest");
    }
    println!("cargo:rerun-if-changed=build.rs");
}
