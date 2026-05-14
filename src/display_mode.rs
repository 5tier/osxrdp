//! macOS display mode switching for RDP aspect ratio matching.
//!
//! IMPORTANT: `CGDisplayMode::all_display_modes` and `CGDisplaySetDisplayMode`
//! both trigger `__os_lock_handoff_lock_slow` crashes (SIGSEGV) after any
//! display mode change. To avoid this, the mode enumeration AND switching
//! are done in a **separate child process**. The parent process never calls
//! any CoreGraphics display mode APIs except `CGDisplay::main()` getters
//! (pixels_wide/pixels_high) which are safe.
//!
//! Usage:
//!   osxrdp --display-mode-switch <display_id> <width> <height>
//!   osxrdp --display-mode-restore <display_id> <width> <height>
//!
//! The child process prints the switched resolution to stdout:
//!   OK <width> <height>

use core_graphics::display::{CGDisplay, CGConfigureOption, CGDisplayMode};
use std::env;
use std::path::PathBuf;
use std::process;
use tracing::{debug, info, warn};

const PENDING_RESTORE_FILE: &str = "/tmp/osxrdp_pending_restore";

/// Check for a pending restore file left by a previous osxrdp run that
/// crashed during a display mode switch. If found, restore the original
/// resolution and delete the file.
pub fn restore_pending() {
    let path = PathBuf::from(PENDING_RESTORE_FILE);
    if !path.exists() {
        return;
    }
    let contents = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Failed to read pending restore file: {e}");
            return;
        }
    };
    // Format: "display_id\noriginal_width\noriginal_height"
    let parts: Vec<&str> = contents.trim().split('\n').collect();
    if parts.len() != 3 {
        eprintln!("Invalid pending restore file format");
        let _ = std::fs::remove_file(&path);
        return;
    }
    let display_id: u32 = match parts[0].parse() {
        Ok(v) => v,
        Err(_) => {
            eprintln!("Invalid display_id in pending restore file");
            let _ = std::fs::remove_file(&path);
            return;
        }
    };
    let original_width: u64 = match parts[1].parse() {
        Ok(v) => v,
        Err(_) => {
            eprintln!("Invalid width in pending restore file");
            let _ = std::fs::remove_file(&path);
            return;
        }
    };
    let original_height: u64 = match parts[2].parse() {
        Ok(v) => v,
        Err(_) => {
            eprintln!("Invalid height in pending restore file");
            let _ = std::fs::remove_file(&path);
            return;
        }
    };

    eprintln!("Found pending display restore: {}×{} on display {}", original_width, original_height, display_id);
    let mut switcher = DisplayModeSwitcher::new_with_id(display_id, original_width, original_height);
    if let Err(e) = switcher.restore() {
        eprintln!("Failed to restore display from crash: {e}");
    }
    let _ = std::fs::remove_file(&path);
}

/// Manages switching the primary macOS display to a resolution that matches
/// the RDP client's desired aspect ratio.
///
/// On creation, saves the current display mode. `switch_to_best_mode()` finds
/// and applies the closest matching mode via a child process. On drop, restores
/// the original mode.
///
/// IMPORTANT: The parent process never calls `CGDisplayMode::all_display_modes`
/// or `CGDisplaySetDisplayMode`. These trigger crashes after display mode
/// changes. All display mode APIs are called in the child process.
pub struct DisplayModeSwitcher {
    display_id: u32,
    /// Original display width (logical) saved for restore.
    original_width: u64,
    /// Original display height (logical) saved for restore.
    original_height: u64,
    switched: bool,
}

impl DisplayModeSwitcher {
    /// Create a switcher targeting the main display.
    /// Uses CGDisplay::main() which is safe (no lock contention).
    pub fn new() -> Self {
        let display = CGDisplay::main();
        // Use pixels_wide/pixels_high instead of display_mode() to avoid
        // the CoreGraphics lock that can crash after mode changes.
        let orig_w = display.pixels_wide() as u64;
        let orig_h = display.pixels_high() as u64;
        debug!(
            "DisplayModeSwitcher: saved original mode {}×{}",
            orig_w, orig_h,
        );
        Self {
            display_id: display.id,
            original_width: orig_w,
            original_height: orig_h,
            switched: false,
        }
    }

    /// Create a switcher with known original dimensions (for restore_pending).
    fn new_with_id(display_id: u32, original_width: u64, original_height: u64) -> Self {
        Self {
            display_id,
            original_width,
            original_height,
            switched: true, // we're restoring, so mark as switched
        }
    }

    /// Try to switch the display to a mode matching `target_w × target_h`.
    ///
    /// Returns `Some((w, h))` with the actual switched resolution, or `None`
    /// if no suitable mode was found or the switch failed.
    ///
    /// ALL CoreGraphics display mode APIs are called in a child process to
    /// avoid the `__os_lock_handoff_lock_slow` crash that occurs after any
    /// display mode change.
    pub fn switch_to_best_mode(&mut self, target_w: u32, target_h: u32) -> Option<(u32, u32)> {
        // Check if already at the target resolution using safe CGDisplay getters.
        let display = CGDisplay::new(self.display_id);
        let current_w = display.pixels_wide() as u32;
        let current_h = display.pixels_high() as u32;

        if current_w == target_w && current_h == target_h {
            debug!("Already at target resolution {}×{}, no switch needed", target_w, target_h);
            return Some((current_w, current_h));
        }

        info!(
            "Requesting display mode switch to {}×{} (current is {}×{})",
            target_w, target_h, current_w, current_h,
        );

        // Spawn a child process that will:
        // 1. Enumerate display modes
        // 2. Find the best match
        // 3. Switch to it
        // 4. Print "OK <width> <height>" to stdout
        let self_exe = env::current_exe().unwrap_or_else(|e| {
            warn!("Cannot determine self executable path: {e}");
            PathBuf::from("osxrdp")
        });

        let output = process::Command::new(self_exe)
            .arg("--display-mode-switch")
            .arg(format!("{}", self.display_id))
            .arg(format!("{}", target_w))
            .arg(format!("{}", target_h))
            .output()
            .map_err(|e| {
                warn!("Failed to spawn display mode helper: {e}");
                format!("Failed to spawn display mode helper: {e}")
            })
            .ok();

        let output = match output {
            Some(o) => o,
            None => return None,
        };

        if !output.status.success() {
            warn!(
                "Display mode helper failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
            return None;
        }

        // Parse "OK <width> <height>" from stdout
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let parts: Vec<&str> = stdout.split_whitespace().collect();
        if parts.len() == 3 && parts[0] == "OK" {
            if let (Ok(w), Ok(h)) = (parts[1].parse::<u32>(), parts[2].parse::<u32>()) {
                self.switched = true;
                // Write pending-restore file
                let _ = std::fs::write(
                    PENDING_RESTORE_FILE,
                    format!("{}\n{}\n{}", self.display_id, self.original_width, self.original_height),
                );
                Some((w, h))
            } else {
                warn!("Invalid resolution in helper output: {}", stdout);
                None
            }
        } else {
            warn!("Unexpected helper output: {}", stdout);
            None
        }
    }

    /// Restore the display to its original mode.
    pub fn restore(&mut self) -> Result<(), String> {
        if !self.switched {
            return Ok(());
        }
        self.switched = false;

        if self.original_width == 0 || self.original_height == 0 {
            return Err("No original mode saved".to_string());
        }

        let self_exe = env::current_exe().unwrap_or_else(|e| {
            warn!("Cannot determine self executable path: {e}");
            PathBuf::from("osxrdp")
        });

        let status = process::Command::new(self_exe)
            .arg("--display-mode-restore")
            .arg(format!("{}", self.display_id))
            .arg(format!("{}", self.original_width))
            .arg(format!("{}", self.original_height))
            .status()
            .map_err(|e| format!("Failed to spawn display mode helper for restore: {e}"))
            .map_err(|e| format!("Failed to spawn display mode helper for restore: {e}"))?;

        if status.success() {
            info!(
                "Restored display to original mode {}×{}",
                self.original_width, self.original_height,
            );
            let _ = std::fs::remove_file(PENDING_RESTORE_FILE);
            Ok(())
        } else {
            Err(format!("Display mode restore helper exited with: {status}"))
        }
    }
}

impl Drop for DisplayModeSwitcher {
    fn drop(&mut self) {
        if let Err(e) = self.restore() {
            warn!("DisplayModeSwitcher drop: {e}");
        }
    }
}

// ─── Child process for display mode switching ───────────────────────────────

/// Run as a standalone helper for display mode switching.
/// Called by osxrdp itself via `std::process::Command` to avoid calling
/// CoreGraphics display mode APIs in the parent process (which would crash
/// after any display mode change due to `__os_lock_handoff_lock_slow`).
///
/// Usage:
///   osxrdp --display-mode-switch <display_id> <target_width> <target_height>
///   osxrdp --display-mode-restore <display_id> <width> <height>
///
/// For --display-mode-switch, prints "OK <width> <height>" to stdout
/// on success.
pub fn run_helper() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 5 {
        eprintln!("Usage: osxrdp --display-mode-switch <display_id> <target_width> <target_height>");
        eprintln!("       osxrdp --display-mode-restore <display_id> <width> <height>");
        process::exit(1);
    }

    let display_id: u32 = match args[2].parse() {
        Ok(v) => v,
        Err(_) => {
            eprintln!("Invalid display_id: {}", args[2]);
            process::exit(1);
        }
    };

    let display = CGDisplay::new(display_id);

    if args[1] == "--display-mode-switch" {
        let target_w: u32 = match args[3].parse() {
            Ok(v) => v,
            Err(_) => {
                eprintln!("Invalid width: {}", args[3]);
                process::exit(1);
            }
        };
        let target_h: u32 = match args[4].parse() {
            Ok(v) => v,
            Err(_) => {
                eprintln!("Invalid height: {}", args[4]);
                process::exit(1);
            }
        };

        // Enumerate display modes and find the best match
        let all_modes = match CGDisplayMode::all_display_modes(display_id, std::ptr::null()) {
            Some(m) => m,
            None => {
                eprintln!("Failed to enumerate display modes");
                process::exit(1);
            }
        };

        let target_ratio = target_w as f64 / target_h as f64;
        let target_pixels = target_w as f64 * target_h as f64;

        let current = display.display_mode();
        let current_w = current.as_ref().map(|m| m.width()).unwrap_or(0);
        let current_h = current.as_ref().map(|m| m.height()).unwrap_or(0);

        eprintln!(
            "Looking for display mode matching {}×{} (ratio {:.4}), current is {}×{}",
            target_w, target_h, target_ratio, current_w, current_h,
        );

        // Already at the target resolution
        if current_w == target_w as u64 && current_h == target_h as u64 {
            println!("OK {} {}", current_w, current_h);
            return;
        }

        let ratio_tolerance = 0.02;

        let mut best_mode: Option<CGDisplayMode> = None;
        let mut best_ratio_diff = f64::MAX;
        let mut best_pixel_diff = f64::MAX;

        for mode in &all_modes {
            let mode_w = mode.width() as u32;
            let mode_h = mode.height() as u32;
            if mode_h == 0 || mode_w == 0 {
                continue;
            }
            let mode_ratio = mode_w as f64 / mode_h as f64;
            let mode_pixels = mode_w as f64 * mode_h as f64;
            let ratio_diff = (mode_ratio - target_ratio).abs();
            let pixel_diff = (mode_pixels - target_pixels).abs();

            if mode_pixels < target_pixels * 0.25 {
                continue;
            }

            if ratio_diff < ratio_tolerance {
                if ratio_diff < best_ratio_diff - 0.001
                    || (ratio_diff < best_ratio_diff + 0.001 && pixel_diff < best_pixel_diff)
                {
                    best_ratio_diff = ratio_diff;
                    best_pixel_diff = pixel_diff;
                    best_mode = Some(mode.clone());
                }
            }
        }

        // If no mode within tolerance, relax and pick closest ratio
        if best_mode.is_none() {
            eprintln!("No mode within ratio tolerance, searching all modes");
            for mode in &all_modes {
                let mode_w = mode.width() as u32;
                let mode_h = mode.height() as u32;
                if mode_h == 0 || mode_w == 0 {
                    continue;
                }
                let mode_ratio = mode_w as f64 / mode_h as f64;
                let mode_pixels = mode_w as f64 * mode_h as f64;
                let ratio_diff = (mode_ratio - target_ratio).abs();
                let pixel_diff = (mode_pixels - target_pixels).abs();

                if mode_pixels < target_pixels * 0.25 {
                    continue;
                }

                if ratio_diff < best_ratio_diff
                    || (ratio_diff < best_ratio_diff + 0.001 && pixel_diff < best_pixel_diff)
                {
                    best_ratio_diff = ratio_diff;
                    best_pixel_diff = pixel_diff;
                    best_mode = Some(mode.clone());
                }
            }
        }

        let best = match best_mode {
            Some(m) => m,
            None => {
                eprintln!("No matching display mode found for {}×{}", target_w, target_h);
                process::exit(1);
            }
        };

        let best_w = best.width() as u32;
        let best_h = best.height() as u32;

        // Already at the best mode
        if best_w == current_w as u32 && best_h == current_h as u32 {
            println!("OK {} {}", best_w, best_h);
            return;
        }

        eprintln!(
            "Switching display from {}×{} to {}×{} (ratio diff: {:.4})",
            current_w, current_h, best_w, best_h, best_ratio_diff,
        );

        // Apply the mode switch
        let config = display.begin_configuration()
            .unwrap_or_else(|e| {
                eprintln!("CGBeginDisplayConfiguration failed: {e}");
                process::exit(1);
            });

        display.configure_display_with_display_mode(&config, &best)
            .unwrap_or_else(|e| {
                eprintln!("CGConfigureDisplayWithDisplayMode failed: {e}");
                process::exit(1);
            });

        display.complete_configuration(&config, CGConfigureOption::ConfigureForSession)
            .unwrap_or_else(|e| {
                eprintln!("CGCompleteDisplayConfiguration failed: {e}");
                process::exit(1);
            });

        // Print the actual switched resolution for the parent to read
        println!("OK {} {}", best_w, best_h);

    } else if args[1] == "--display-mode-restore" {
        let orig_w: u64 = match args[3].parse() {
            Ok(v) => v,
            Err(_) => {
                eprintln!("Invalid width: {}", args[3]);
                process::exit(1);
            }
        };
        let orig_h: u64 = match args[4].parse() {
            Ok(v) => v,
            Err(_) => {
                eprintln!("Invalid height: {}", args[4]);
                process::exit(1);
            }
        };

        let all_modes = match CGDisplayMode::all_display_modes(display_id, std::ptr::null()) {
            Some(m) => m,
            None => {
                eprintln!("Failed to enumerate display modes");
                process::exit(1);
            }
        };

        let mode = all_modes.iter().find(|m| {
            m.width() == orig_w && m.height() == orig_h
        }).unwrap_or_else(|| {
            eprintln!("No display mode found for {}×{}", orig_w, orig_h);
            process::exit(1);
        });

        let config = display.begin_configuration()
            .unwrap_or_else(|e| {
                eprintln!("CGBeginDisplayConfiguration failed: {e}");
                process::exit(1);
            });

        display.configure_display_with_display_mode(&config, mode)
            .unwrap_or_else(|e| {
                eprintln!("CGConfigureDisplayWithDisplayMode failed: {e}");
                process::exit(1);
            });

        display.complete_configuration(&config, CGConfigureOption::ConfigureForSession)
            .unwrap_or_else(|e| {
                eprintln!("CGCompleteDisplayConfiguration failed: {e}");
                process::exit(1);
            });

        eprintln!("Restored display {} to {}×{}", display_id, orig_w, orig_h);
    } else {
        eprintln!("Unknown argument: {}", args[1]);
        process::exit(1);
    }
}