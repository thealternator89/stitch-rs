#[cfg(any(target_os = "macos", target_os = "linux"))]
use std::process::{Child, Command, Stdio};

/// Best-effort OS sleep/idle prevention guard active while dropped on macOS (`caffeinate`)
/// and Linux (`systemd-inhibit`).
pub struct SleepGuard {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    child: Option<Child>,
}

impl SleepGuard {
    pub fn acquire() -> Self {
        #[cfg(target_os = "macos")]
        {
            let pid = std::process::id().to_string();
            let child = Command::new("caffeinate")
                .args(["-i", "-w", &pid])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .ok();
            Self { child }
        }

        #[cfg(target_os = "linux")]
        {
            let child = Command::new("systemd-inhibit")
                .args([
                    "--what=idle",
                    "--who=Stitch",
                    "--why=Running PR code review",
                    "--mode=block",
                    "sleep",
                    "86400",
                ])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .ok();
            Self { child }
        }

        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            Self {}
        }
    }
}

impl Drop for SleepGuard {
    fn drop(&mut self) {
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}
