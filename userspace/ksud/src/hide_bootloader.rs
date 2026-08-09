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

/// Properties rewritten to emulate a locked bootloader.
const PROPS_TO_HIDE: &[(&str, &str)] = &[
    // Generic bootloader / verified-boot state
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
    // vbmeta details
    ("ro.boot.vbmeta.invalidate_on_error", "yes"),
    ("ro.boot.vbmeta.avb_version", "1.0"),
    ("ro.boot.vbmeta.hash_alg", "sha256"),
    ("ro.boot.vbmeta.size", "4096"),
    // OEM specific
    ("ro.secureboot.lockstate", "locked"), // MIUI
    ("ro.boot.realmebootstate", "green"),  // Realme
    ("ro.boot.realme.lockstate", "1"),
    ("ro.boot.oem_unlock_support", "0"), // OnePlus
];

/// Bootmode props checked for recovery mode.
const BOOT_KEYS: &[&str] = &["ro.bootmode", "ro.boot.bootmode", "vendor.boot.bootmode"];

pub fn is_bl_hiding_enabled() -> bool {
    Path::new(BL_HIDE_CONFIG).exists()
}

pub fn set_bl_hiding_enabled(enabled: bool) -> Result<()> {
    if enabled {
        crate::utils::ensure_dir_exists(Path::new(defs::WORKING_DIR))
            .context("failed to create working dir")?;
        fs::write(BL_HIDE_CONFIG, "1\n")
            .with_context(|| format!("failed to write {BL_HIDE_CONFIG}"))?;
        info!("hide_bl: enabled");
    } else if Path::new(BL_HIDE_CONFIG).exists() {
        fs::remove_file(BL_HIDE_CONFIG)
            .with_context(|| format!("failed to remove {BL_HIDE_CONFIG}"))?;
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

/// Set `name` to `expected` if it exists and differs.
fn check_reset_prop(name: &str, expected: &str) {
    let Some(value) = crate::utils::getprop(name) else {
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

/// Rewrite bootloader-related props to hide unlock status.
///
/// Called after boot completes so all properties are finalised.
pub fn hide_bootloader_status() {
    if !is_bl_hiding_enabled() {
        return;
    }
    if let Err(e) = sys_prop::init() {
        warn!("hide_bl: failed to init property API: {e:#}");
        return;
    }
    info!("hide_bl: hiding bootloader status");
    for (name, expected) in PROPS_TO_HIDE {
        check_reset_prop(name, expected);
    }
    // bootmode in recovery leaks recovery state
    for key in BOOT_KEYS {
        if let Some(val) = crate::utils::getprop(key)
            && val.contains("recovery")
        {
            info!("hide_bl: resetting {key} (recovery) to unknown");
            if let Err(e) = reset_prop(key, "unknown") {
                warn!("hide_bl: failed to reset {key}: {e:#}");
            }
        }
    }
    info!("hide_bl: done");
}
