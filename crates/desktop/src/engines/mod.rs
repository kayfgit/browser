//! Adapter entry points. The shell only stores browser_engine trait objects.
mod webview2;

#[cfg(test)]
mod tests;

pub(crate) use webview2::{
    build as build_webview2, keep_alive as keep_webview2_alive, BuildOptions as WebView2Options,
};
