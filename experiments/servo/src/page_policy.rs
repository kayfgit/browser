//! Conservative metadata for the lab adapter, not an installed provider entry.
use browser_engine::{Capabilities, EngineResult, ProviderDescriptor};

pub const DESCRIPTOR: ProviderDescriptor = ProviderDescriptor {
    id: "servo",
    family: "servo",
    display_name: "Servo (qualification lab)",
    capabilities: Capabilities {
        private: false,
        disable_javascript: false,
        document_scripts: true,
        // The top-level polling experiment is not yet the full production bridge.
        page_messages: false,
    },
};

pub fn validate_zoom(factor: f64) -> EngineResult {
    if !factor.is_finite() || !(0.1..=10.0).contains(&factor) {
        return Err("Servo zoom must be between 0.1 and 10".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use browser_engine::{StorageMode, ViewRequirements, build_checked};
    #[test]
    fn unsupported_requests_never_construct_a_servo_view() {
        for (storage, disable_javascript, shell_bridge) in [
            (StorageMode::Private, false, false),
            (StorageMode::Persistent, true, false),
            (StorageMode::Persistent, false, true),
        ] {
            let result = build_checked(
                &DESCRIPTOR,
                ViewRequirements {
                    storage,
                    disable_javascript,
                    shell_bridge,
                },
                |_| -> EngineResult<()> {
                    panic!("unsupported request reached Servo construction");
                },
            );
            assert!(result.is_err());
        }
    }
    #[test]
    fn zoom_rejects_nonfinite_and_out_of_range_values() {
        for value in [
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
            0.0,
            -1.0,
            0.09,
            10.01,
        ] {
            assert!(validate_zoom(value).is_err());
        }
        for value in [0.1, 1.0, 1.5, 10.0] {
            assert!(validate_zoom(value).is_ok());
        }
    }
}
