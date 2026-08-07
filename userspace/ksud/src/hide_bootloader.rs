use crate::defs;
use anyhow::{Context, Result};
use const_format::concatcp;
use log::{info, warn};
use prop_rs_android::resetprop::ResetProp;
use prop_rs_android::sys_prop;
use std::fs;
use std::path::Path;

/// Marker file that enables bootloader status hiding at boot.
pub const BL_HIDE_CONFIG: &str = concatcp!(defs::WORKING_DIR, ".hide_bootloader");

/// Properties to force to "locked / green / enforcing" style values.
///
/// Merged from YukiSU and FolkPatch implementations:
/// - generic verified-boot / bootloader state
/// - MIUI (`ro.secureboot.lockstate`)
/// - Realme (`realmebootstate`, `realme.lockstate`)
/// - OnePlus (`oem_unlock_support`)
/// - additional vbmeta details (FolkPatch) so that integrity checks see a
///   coherent, locked device.
const PROPS_TO_HIDE: &[(&str, &str)] = &[
    // Generic bootloader / verified boot status
    ("ro.boot.vbmeta.device_state", "locked"),
    ("ro.boot.verifiedbootstate", "green"),
    ("ro.boot.flash.locked", "1"),
    ("ro.boot.veritymode", "enforcing"),
    ("ro.boot.warranty_bit", "0"),
    ("ro.warranty_bit", "0"),
    ("ro.debuggable", "0"),
    ("ro.force.debuggable", "0"),
    ("ro.secure", "1"),
    ("ro.adb.secure", "1"),
    ("ro.build.type", "user"),
    ("ro.build.tags", "release-keys"),
    ("ro.vendor.boot.warranty_bit", "0"),
    ("ro.vendor.warranty_bit", "0"),
    ("vendor.boot.vbmeta.device_state", "locked"),
    ("vendor.boot.verifiedbootstate", "green"),
    ("sys.oem_unlock_allowed", "0"),
    // Additional vbmeta details (FolkPatch)
    ("ro.boot.vbmeta.invalidate_on_error", "yes"),
    ("ro.boot.vbmeta.avb_version", "1.0"),
    ("ro.boot.vbmeta.hash_alg", "sha256"),
    ("ro.boot.vbmeta.size", "4096"),
    // MIUI specific
    ("ro.secureboot.lockstate", "locked"),
    // Realme specific
    ("ro.boot.realmebootstate", "green"),
    ("ro.boot.realme.lockstate", "1"),
    // OnePlus specific
    ("ro.boot.oem_unlock_support", "0"),
];

/// Bootmode keys checked for recovery mode (FolkPatch behaviour).
const BOOT_KEYS: &[&str] = &["ro.bootmode", "ro.boot.bootmode", "vendor.boot.bootmode"];

/// Check whether bootloader hiding is enabled.
pub fn is_bl_hiding_enabled() -> bool {
    Path::new(BL_HIDE_CONFIG).exists()
}

/// Enable/disable bootloader hiding by creating/removing the marker file.
pub fn set_bl_hiding_enabled(enabled: bool) -> Result<()> {
    if enabled {
        // Ensure the working dir exists (e.g. on first use before ksud setup finished).
        crate::utils::ensure_dir_exists(Path::new(defs::WORKING_DIR))
            .context("failed to create working dir")?;
        fs::write(BL_HIDE_CONFIG, "1\n")
            .with_context(|| format!("failed to write {BL_HIDE_CONFIG}"))?;
        info!("hide_bl: enabled");
    } else {
        if Path::new(BL_HIDE_CONFIG).exists() {
            fs::remove_file(BL_HIDE_CONFIG)
                .with_context(|| format!("failed to remove {BL_HIDE_CONFIG}"))?;
        }
        info!("hide_bl: disabled");
    }
    Ok(())
}

fn reset_prop(name: &str, value: &str) -> Result<()> {
    let rp = ResetProp {
        skip_svc: true,
        persistent: false,
        persist_only: false,
        verbose: false,
        show_context: false,
        rebuild: false,
    };
    rp.set(name, value)
        .with_context(|| format!("failed to set {name} to {value}"))
}

fn get_prop(name: &str) -> Option<String> {
    crate::utils::getprop(name)
}

/// Reset a property if it exists and does not already match the expected value.
fn check_reset_prop(name: &str, expected: &str) {
    let Some(value) = get_prop(name) else {
        // property doesn't exist, nothing to hide
        return;
    };
    if value == expected {
        return;
    }
    info!("hide_bl: resetting {name} from '{value}' to '{expected}'");
    if let Err(e) = reset_prop(name, expected) {
        warn!("hide_bl: failed to reset {name}: {e:#}");
    }
}

/// Hide bootloader unlock status by resetting system properties.
///
/// This is a "soft" hiding method: it rewrites the properties that apps and
/// system services probe to detect an unlocked bootloader. Should be called
/// after boot completes so all properties are finalised.
pub fn hide_bootloader_status() {
    if !is_bl_hiding_enabled() {
        info!("hide_bl: disabled, skipping");
        return;
    }

    if let Err(e) = sys_prop::init() {
        warn!("hide_bl: failed to initialize property API: {e:#}");
        return;
    }

    info!("hide_bl: starting bootloader status hiding");
    for (name, expected) in PROPS_TO_HIDE {
        check_reset_prop(name, expected);
    }

    // Avoid leaking recovery mode through bootmode properties.
    for key in BOOT_KEYS {
        if let Some(val) = get_prop(key) {
            if val.contains("recovery") {
                info!("hide_bl: resetting {key} (recovery) to unknown");
                if let Err(e) = reset_prop(key, "unknown") {
                    warn!("hide_bl: failed to reset {key}: {e:#}");
                }
            }
        }
    }

    info!("hide_bl: bootloader status hiding completed");
}
