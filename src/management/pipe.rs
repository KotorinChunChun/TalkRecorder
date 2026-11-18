//! 利用者SIDに限定した名前付きパイプと期限付きI/O。
use anyhow::{Context, ensure};
use std::io;
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Security::Authorization::*;
use windows_sys::Win32::Security::*;
use windows_sys::Win32::Storage::FileSystem::*;
use windows_sys::Win32::System::IO::*;
use windows_sys::Win32::System::Pipes::*;
use windows_sys::Win32::System::RemoteDesktop::ProcessIdToSessionId;
use windows_sys::Win32::System::Threading::*;

struct OwnedHandle(HANDLE);
impl Drop for OwnedHandle {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

struct LocalMemory(HLOCAL);
impl Drop for LocalMemory {
    fn drop(&mut self) {
        unsafe { LocalFree(self.0) };
    }
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}

fn current_user_sid() -> anyhow::Result<String> {
    let mut token = null_mut();
    ensure!(
        unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } != 0,
        "利用者トークンを開けません"
    );
    let _token = OwnedHandle(token);
    let mut length = 0;
    unsafe { GetTokenInformation(token, TokenUser, null_mut(), 0, &mut length) };
    ensure!(length > 0, "利用者SIDのサイズを取得できません");
    let mut buffer = vec![0usize; (length as usize).div_ceil(size_of::<usize>())];
    ensure!(
        unsafe {
            GetTokenInformation(
                token,
                TokenUser,
                buffer.as_mut_ptr().cast(),
                length,
                &mut length,
            )
        } != 0,
        "利用者SIDを取得できません"
    );
    let user = unsafe { &*buffer.as_ptr().cast::<TOKEN_USER>() };
    let mut sid = null_mut();
    ensure!(
        unsafe { ConvertSidToStringSidW(user.User.Sid, &mut sid) } != 0,
        "利用者SIDを変換できません"
    );
    let _sid = LocalMemory(sid.cast());
    let mut count = 0;
    while unsafe { *sid.add(count) } != 0 {
        count += 1;
    }
    Ok(String::from_utf16(unsafe {
        std::slice::from_raw_parts(sid, count)
    })?)
}

pub(super) struct ManagementPipe {
    handle: OwnedHandle,
}

impl ManagementPipe {
    pub(super) fn create(id: &str) -> anyhow::Result<Self> {
        let sid = current_user_sid()?;
        let name = wide(&format!(r"\\.\pipe\KotorinToys.{sid}.{id}"));
        // 現在の利用者だけを許可し、.NET CurrentUserOnlyのACL検査と揃える。
        let sddl = wide(&format!("O:{sid}D:P(A;;GA;;;{sid})"));
        let mut descriptor = null_mut();
        ensure!(
            unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    sddl.as_ptr(),
                    SDDL_REVISION_1,
                    &mut descriptor,
                    null_mut(),
                )
            } != 0,
            "管理窓口のACLを作成できません"
        );
        let _descriptor = LocalMemory(descriptor.cast());
        let security = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor.cast(),
            bInheritHandle: 0,
        };
        let handle = unsafe {
            CreateNamedPipeW(
                name.as_ptr(),
                PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED | FILE_FLAG_FIRST_PIPE_INSTANCE,
                PIPE_TYPE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                1,
                8192,
                8192,
                0,
                &security,
            )
        };
        ensure!(
            handle != INVALID_HANDLE_VALUE,
            "管理窓口を作成できません: {}",
            unsafe { GetLastError() }
        );
        Ok(Self {
            handle: OwnedHandle(handle),
        })
    }

    pub(super) fn accept(&self, shutdown: &AtomicBool) -> anyhow::Result<bool> {
        let mut operation = self.operation()?;
        let started = unsafe { ConnectNamedPipe(self.handle.0, &mut operation.0) };
        if started != 0 {
            return Ok(true);
        }
        match unsafe { GetLastError() } {
            ERROR_PIPE_CONNECTED => Ok(true),
            ERROR_IO_PENDING => self.wait_for_connection(&mut operation.0, shutdown),
            code => self.handle_connect_error(code),
        }
    }

    fn wait_for_connection(
        &self,
        operation: &mut OVERLAPPED,
        shutdown: &AtomicBool,
    ) -> anyhow::Result<bool> {
        // 通常の待機で接続I/Oを取り消さない。取消しと接続完了が競合するのは終了時だけ。
        while !shutdown.load(Ordering::Acquire) {
            match unsafe { WaitForSingleObject(operation.hEvent, 100) } {
                WAIT_OBJECT_0 => {
                    return match self.io_result(operation, false) {
                        Ok(_) => Ok(true),
                        Err(error) => self
                            .handle_connect_error(error.raw_os_error().unwrap_or_default() as u32),
                    };
                }
                WAIT_TIMEOUT => {}
                _ => {
                    let error = io::Error::last_os_error();
                    self.cancel_and_wait(operation);
                    return Err(error).context("管理接続を待機できません");
                }
            }
        }
        self.cancel_and_wait(operation);
        Ok(false)
    }

    fn handle_connect_error(&self, code: u32) -> anyhow::Result<bool> {
        match code {
            ERROR_NO_DATA | ERROR_BROKEN_PIPE | ERROR_PIPE_NOT_CONNECTED => {
                // 相手が先に閉じた場合も、サーバー側を切断状態に戻して次を受け付ける。
                self.disconnect();
                Ok(false)
            }
            _ => Err(io::Error::from_raw_os_error(code as i32)).context("管理窓口に接続できません"),
        }
    }

    pub(super) fn same_session(&self) -> anyhow::Result<bool> {
        let mut client = 0;
        let mut client_session = 0;
        let mut own_session = 0;
        ensure!(
            unsafe { GetNamedPipeClientProcessId(self.handle.0, &mut client) } != 0,
            "クライアントPIDが不明です"
        );
        ensure!(
            unsafe { ProcessIdToSessionId(client, &mut client_session) } != 0
                && unsafe { ProcessIdToSessionId(std::process::id(), &mut own_session) } != 0,
            "対話セッションが不明です"
        );
        Ok(client_session == own_session)
    }

    pub(super) fn read_line(&self, deadline: Instant) -> anyhow::Result<String> {
        let mut bytes = Vec::new();
        loop {
            ensure!(Instant::now() < deadline, "管理要求が時間切れです");
            let mut buffer = [0u8; 512];
            let mut operation = self.operation()?;
            let started = unsafe {
                ReadFile(
                    self.handle.0,
                    buffer.as_mut_ptr(),
                    buffer.len() as u32,
                    null_mut(),
                    &mut operation.0,
                )
            };
            let count = self.finish(
                started,
                &mut operation.0,
                deadline.saturating_duration_since(Instant::now()),
            )? as usize;
            ensure!(count != 0, "空の管理要求です");
            bytes.extend_from_slice(&buffer[..count]);
            ensure!(bytes.len() <= 8192, "管理要求が長すぎます");
            if let Some(end) = bytes.iter().position(|byte| *byte == b'\n') {
                return Ok(String::from_utf8(bytes[..end].to_vec())?
                    .trim_end_matches('\r')
                    .trim_start_matches('\u{feff}')
                    .to_owned());
            }
        }
    }

    pub(super) fn write_line(&self, line: &str, deadline: Instant) -> anyhow::Result<()> {
        let bytes = format!("{line}\n").into_bytes();
        let mut written = 0;
        while written < bytes.len() {
            ensure!(Instant::now() < deadline, "管理応答が時間切れです");
            let mut operation = self.operation()?;
            let started = unsafe {
                WriteFile(
                    self.handle.0,
                    bytes[written..].as_ptr(),
                    (bytes.len() - written) as u32,
                    null_mut(),
                    &mut operation.0,
                )
            };
            let count = self.finish(
                started,
                &mut operation.0,
                deadline.saturating_duration_since(Instant::now()),
            )? as usize;
            ensure!(count > 0, "管理応答を書き込めません");
            written += count;
        }
        Ok(())
    }

    fn operation(&self) -> anyhow::Result<(OVERLAPPED, OwnedHandle)> {
        let event = unsafe { CreateEventW(null(), 1, 0, null()) };
        ensure!(!event.is_null(), "I/Oイベントを作成できません");
        let mut operation: OVERLAPPED = unsafe { std::mem::zeroed() };
        operation.hEvent = event;
        Ok((operation, OwnedHandle(event)))
    }

    fn finish(
        &self,
        started: i32,
        operation: &mut OVERLAPPED,
        timeout: Duration,
    ) -> anyhow::Result<u32> {
        if started == 0 {
            ensure!(
                unsafe { GetLastError() } == ERROR_IO_PENDING,
                "管理I/Oを開始できません"
            );
            if unsafe {
                WaitForSingleObject(
                    operation.hEvent,
                    timeout.as_millis().min(u32::MAX as u128) as u32,
                )
            } != WAIT_OBJECT_0
            {
                self.cancel_and_wait(operation);
                anyhow::bail!("管理I/Oが時間切れです");
            }
        }
        self.io_result(operation, false)
            .context("管理I/Oを完了できません")
    }

    fn cancel_and_wait(&self, operation: &mut OVERLAPPED) {
        // バッファとOVERLAPPEDを破棄する前に、取消しまたはI/O完了を回収する。
        unsafe { CancelIoEx(self.handle.0, operation) };
        let _ = self.io_result(operation, true);
    }

    fn io_result(&self, operation: &mut OVERLAPPED, wait: bool) -> io::Result<u32> {
        let mut count = 0;
        if unsafe { GetOverlappedResult(self.handle.0, operation, &mut count, wait as i32) } != 0 {
            Ok(count)
        } else {
            Err(io::Error::last_os_error())
        }
    }

    pub(super) fn wait_for_client_close(&self, deadline: Instant) {
        while Instant::now() < deadline {
            if unsafe {
                PeekNamedPipe(
                    self.handle.0,
                    null_mut(),
                    0,
                    null_mut(),
                    null_mut(),
                    null_mut(),
                )
            } == 0
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    pub(super) fn disconnect(&self) {
        unsafe { DisconnectNamedPipe(self.handle.0) };
    }
}

#[cfg(test)]
pub(super) fn test_client(id: &str, timeout: Duration) -> anyhow::Result<std::fs::File> {
    let sid = current_user_sid()?;
    let name = format!(r"\\.\pipe\KotorinToys.{sid}.{id}");
    let deadline = Instant::now() + timeout;
    loop {
        match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&name)
        {
            Ok(client) => return Ok(client),
            Err(error)
                if error.raw_os_error() == Some(ERROR_PIPE_BUSY as i32)
                    && Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => return Err(error).context("試験クライアントが接続できません"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reconnects_after_client_opens_and_closes_before_accept() -> anyhow::Result<()> {
        let id = format!("talk-recorder-abandoned-{}", std::process::id());
        let server = ManagementPipe::create(&id)?;
        let shutdown = AtomicBool::new(false);
        drop(test_client(&id, Duration::ZERO)?);
        assert!(!server.accept(&shutdown)?);

        std::thread::scope(|scope| -> anyhow::Result<()> {
            let next = scope.spawn(|| {
                let client = test_client(&id, Duration::from_secs(2));
                if client.is_err() {
                    shutdown.store(true, Ordering::Release);
                }
                client
            });
            assert!(server.accept(&shutdown)?);
            let _client = next.join().expect("接続試験スレッド")?;
            assert!(server.same_session()?);
            Ok(())
        })
    }
}
