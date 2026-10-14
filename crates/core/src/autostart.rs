//! 开机自动启动：写入当前用户的 `Run` 注册表项。
//!
//! 只动 `HKEY_CURRENT_USER\Software\Microsoft\Windows\CurrentVersion\Run`，
//! 不需要管理员权限；非 Windows 平台提供空实现，保证整体可编译。

/// Run 项下的值名。
const RUN_VALUE: &str = "zcode-board";

/// 当前是否设置了开机启动。
pub fn is_enabled() -> bool {
    imp::is_enabled()
}

/// 打开或关闭开机启动。返回操作后的实际状态（以注册表回读为准）。
pub fn set_enabled(on: bool) -> bool {
    imp::set_enabled(on)
}

/// 当前可执行文件路径（带引号，注册表值要求）。
pub fn exe_command() -> Option<String> {
    std::env::current_exe()
        .ok()
        .map(|p| format!("\"{}\"", p.display()))
}

#[cfg(windows)]
mod imp {
    use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS, WIN32_ERROR};
    use windows_sys::Win32::System::Registry::{
        HKEY, HKEY_CURRENT_USER, KEY_READ, KEY_WRITE, REG_SZ, RegCloseKey, RegDeleteValueW,
        RegOpenKeyExW, RegQueryValueExW, RegSetValueExW,
    };

    const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// 打开 Run 键。`write` 决定以只读还是读写方式打开。
    fn open_run_key(write: bool) -> Option<HKEY> {
        let path = wide(RUN_KEY);
        let access = if write { KEY_WRITE } else { KEY_READ };
        let mut key: HKEY = std::ptr::null_mut();
        // SAFETY: path 是以 NUL 结尾的合法 UTF-16，key 出参由我们初始化后释放。
        let rc: WIN32_ERROR = unsafe {
            RegOpenKeyExW(HKEY_CURRENT_USER, path.as_ptr(), 0, access, &mut key)
        };
        (rc == ERROR_SUCCESS).then_some(key)
    }

    pub fn is_enabled() -> bool {
        // 值存在且指向当前 exe 才算开启：exe 挪过位置（如从 target 拷到 app）
        // 后留下的旧路径值是死的，视为未开启，由启动自愈用当前路径重写。
        let Some(cmd) = query_run_value() else {
            return false;
        };
        match super::exe_command() {
            Some(cur) => normalize_cmd(&cmd) == normalize_cmd(&cur),
            None => true,
        }
    }

    /// 读取 Run 项下本工具值的内容（启动命令）。不存在时返回 None。
    fn query_run_value() -> Option<String> {
        let key = open_run_key(false)?;
        let name = wide(super::RUN_VALUE);
        let mut buf = [0u16; 1024];
        let mut size = (buf.len() * 2) as u32;
        // SAFETY: name 合法，buf/size 初始化为缓冲区实样。
        let rc: WIN32_ERROR = unsafe {
            RegQueryValueExW(
                key,
                name.as_ptr(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                buf.as_mut_ptr() as *mut u8,
                &mut size,
            )
        };
        // SAFETY: key 来自 open_run_key，未重复关闭。
        unsafe { RegCloseKey(key) };
        if rc != ERROR_SUCCESS {
            return None;
        }
        let len = ((size as usize) / 2).min(buf.len());
        let end = buf[..len].iter().position(|&c| c == 0).unwrap_or(len);
        Some(String::from_utf16_lossy(&buf[..end]))
    }

    pub fn set_enabled(on: bool) -> bool {
        let Some(key) = open_run_key(true) else {
            tracing::warn!("无法打开 Run 注册表项，开机启动设置失败");
            return false;
        };
        let name = wide(super::RUN_VALUE);

        let rc: WIN32_ERROR = if on {
            let Some(cmd) = super::exe_command() else {
                // SAFETY: key 来自 open_run_key，未重复关闭。
                unsafe { RegCloseKey(key) };
                return false;
            };
            let value = wide(&cmd);
            let bytes =
                unsafe { std::slice::from_raw_parts(value.as_ptr() as *const u8, value.len() * 2) };
            // SAFETY: name/value 合法，bytes 长度与 value 一致。
            unsafe {
                RegSetValueExW(
                    key,
                    name.as_ptr(),
                    0,
                    REG_SZ,
                    bytes.as_ptr(),
                    bytes.len() as u32,
                )
            }
        } else {
            // SAFETY: name 合法；值不存在时返回错误码，由下方统一处理。
            unsafe { RegDeleteValueW(key, name.as_ptr()) }
        };

        // SAFETY: key 来自 open_run_key，未重复关闭。
        unsafe { RegCloseKey(key) };

        // 删除时「值本来就不存在」也算成功；最终以回读结果为准，
        // 避免调用方（尤其是界面）拿到过期状态。
        let ok = rc == ERROR_SUCCESS || (!on && rc == ERROR_FILE_NOT_FOUND);
        if !ok {
            tracing::warn!(?rc, "写入开机启动注册表失败");
        }
        if on { ok } else { is_enabled() }
    }

    /// 归一化启动命令以便比较：去引号、去空白、大小写与斜杠方向统一。
    fn normalize_cmd(s: &str) -> String {
        s.trim()
            .trim_matches('"')
            .to_lowercase()
            .replace('/', "\\")
    }
}

#[cfg(not(windows))]
mod imp {
    pub fn is_enabled() -> bool {
        false
    }
    pub fn set_enabled(_on: bool) -> bool {
        false
    }
}

#[cfg(windows)]
#[cfg(test)]
mod tests {
    use super::*;

    /// 注册表往返：关闭 → 应读不到；开启 → 应读到当前 exe。
    /// 用例跑完会把用户原来的值原样恢复。
    #[test]
    fn registry_roundtrip() {
        // 备份原值（不存在则为 None）。
        let original = is_enabled().then(exe_command).flatten();

        assert!(!set_enabled(false), "关闭后应处于未开启状态");
        assert!(!is_enabled(), "关闭后 is_enabled 应为 false");

        assert!(set_enabled(true), "开启后应处于已开启状态");
        assert!(is_enabled(), "开启后 is_enabled 应为 true");
        let current = is_enabled().then(exe_command).flatten();
        assert!(current.is_some(), "开启后注册表应有值");

        // 恢复原状。
        if let Some(original) = original {
            let _ = set_enabled_force(&original);
        } else {
            let _ = set_enabled(false);
        }
    }

    /// 测试专用：直接写入指定命令（绕过 current_exe，方便恢复现场）。
    fn set_enabled_force(command: &str) -> bool {
        use windows_sys::Win32::System::Registry::{
            HKEY_CURRENT_USER, KEY_WRITE, REG_SZ, RegCloseKey, RegOpenKeyExW, RegSetValueExW,
        };
        let path: Vec<u16> = "Software\\Microsoft\\Windows\\CurrentVersion\\Run"
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let name: Vec<u16> = RUN_VALUE.encode_utf16().chain(std::iter::once(0)).collect();
        let value: Vec<u16> = command.encode_utf16().chain(std::iter::once(0)).collect();
        unsafe {
            let mut key = std::ptr::null_mut();
            if RegOpenKeyExW(HKEY_CURRENT_USER, path.as_ptr(), 0, KEY_WRITE, &mut key) != 0 {
                return false;
            }
            let bytes = std::slice::from_raw_parts(value.as_ptr() as *const u8, value.len() * 2);
            let rc = RegSetValueExW(
                key,
                name.as_ptr(),
                0,
                REG_SZ,
                bytes.as_ptr(),
                bytes.len() as u32,
            );
            RegCloseKey(key);
            rc == 0
        }
    }

    #[test]
    fn exe_command_is_quoted() {
        let cmd = exe_command().expect("测试二进制应有路径");
        assert!(cmd.starts_with('"') && cmd.ends_with('"'), "路径应带引号: {cmd}");
    }
}
