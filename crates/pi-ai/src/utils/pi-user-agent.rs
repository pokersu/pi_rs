//! Rust 翻译自 packages/ai/src/utils/pi-user-agent.ts
//!
//! 构造 `pi` 的 User-Agent 字符串。

/// 对应 `getPiUserAgent()`：`pi (<platform> <release>; <arch>)`。
pub fn get_pi_user_agent() -> String {
    let platform = std::env::consts::OS;
    let arch = std::env::consts::ARCH;
    let release = os_release();
    if release.is_empty() {
        format!("pi ({platform}; {arch})")
    } else {
        format!("pi ({platform} {release}; {arch})")
    }
}

/// 尽力获取内核 release 字符串；失败时返回空字符串。
#[cfg(target_os = "linux")]
fn os_release() -> String {
    std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

#[cfg(not(target_os = "linux"))]
fn os_release() -> String {
    // macOS/Windows 上无轻量等价物，返回空字符串。
    String::new()
}
