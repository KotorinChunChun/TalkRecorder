use eframe::egui;
use std::sync::atomic::{AtomicU8, Ordering};
#[cfg(windows)]
use std::sync::{Arc, atomic::AtomicBool};
use tray_icon::{
    MouseButton, TrayIconEvent,
    menu::{MenuEvent, MenuId},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DesktopAction {
    Show = 1,
    Exit = 2,
}

#[derive(Default)]
pub(crate) struct DesktopRequests {
    pending: AtomicU8,
    #[cfg(windows)]
    window: usize,
    #[cfg(windows)]
    keep_hidden: Arc<AtomicBool>,
}

impl DesktopRequests {
    pub(crate) fn new(cc: &eframe::CreationContext<'_>, hidden: bool) -> anyhow::Result<Self> {
        #[cfg(windows)]
        {
            use raw_window_handle::{HasWindowHandle, RawWindowHandle};
            let RawWindowHandle::Win32(handle) = cc.window_handle()?.as_raw() else {
                anyhow::bail!("設定画面のWindowsハンドルを取得できません");
            };
            let requests = Self {
                window: handle.hwnd.get() as usize,
                ..Self::default()
            };
            if hidden {
                requests.keep_hidden.store(true, Ordering::Release);
                let state = Arc::into_raw(requests.keep_hidden.clone());
                // eframe の初回描画は Visible(false) に関わらず表示を要求する。
                let installed = unsafe {
                    windows_sys::Win32::UI::Shell::SetWindowSubclass(
                        requests.window as _,
                        Some(startup_visibility),
                        1,
                        state as usize,
                    )
                };
                if installed == 0 {
                    unsafe { drop(Arc::from_raw(state)) };
                    anyhow::bail!("起動時のウィンドウ非表示を準備できません");
                }
            }
            Ok(requests)
        }
        #[cfg(not(windows))]
        {
            let _ = (cc, hidden);
            Ok(Self::default())
        }
    }

    pub(crate) fn send(&self, ctx: &egui::Context, action: DesktopAction) {
        // 終了と表示が競合しても、終了要求を上書きしない。
        self.pending.fetch_max(action as u8, Ordering::AcqRel);
        // 非表示ウィンドウでは描画が止まるため、updateより先に表示を要求する。
        #[cfg(windows)]
        if self.window != 0 {
            self.keep_hidden.store(false, Ordering::Release);
            use windows::Win32::{
                Foundation::HWND,
                UI::WindowsAndMessaging::{IsIconic, SW_RESTORE, SW_SHOW, ShowWindowAsync},
            };
            let window = HWND(self.window as *mut _);
            // 自身のウィンドウだけを起こす。実機入力や他プロセスの操作はしない。
            unsafe {
                let command = if IsIconic(window).as_bool() {
                    SW_RESTORE
                } else {
                    SW_SHOW
                };
                let _ = ShowWindowAsync(window, command);
            }
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
        if action == DesktopAction::Show {
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        }
        ctx.request_repaint();
    }

    pub(crate) fn take(&self) -> Option<DesktopAction> {
        match self.pending.swap(0, Ordering::AcqRel) {
            1 => Some(DesktopAction::Show),
            2 => Some(DesktopAction::Exit),
            _ => None,
        }
    }
}

#[cfg(windows)]
unsafe extern "system" fn startup_visibility(
    window: windows_sys::Win32::Foundation::HWND,
    message: u32,
    wparam: usize,
    lparam: isize,
    id: usize,
    data: usize,
) -> isize {
    use windows_sys::Win32::UI::{
        Shell::{DefSubclassProc, RemoveWindowSubclass},
        WindowsAndMessaging::{
            GWL_STYLE, STYLESTRUCT, SWP_NOACTIVATE, SWP_SHOWWINDOW, WINDOWPOS, WM_NCDESTROY,
            WM_STYLECHANGING, WM_WINDOWPOSCHANGING, WS_VISIBLE,
        },
    };
    let state = data as *const AtomicBool;
    if message == WM_NCDESTROY {
        unsafe { RemoveWindowSubclass(window, Some(startup_visibility), id) };
        unsafe { drop(Arc::from_raw(state)) };
    } else if lparam != 0 && unsafe { (*state).load(Ordering::Acquire) } {
        if message == WM_WINDOWPOSCHANGING {
            let position = unsafe { &mut *(lparam as *mut WINDOWPOS) };
            if position.flags & SWP_SHOWWINDOW != 0 {
                position.flags = (position.flags & !SWP_SHOWWINDOW) | SWP_NOACTIVATE;
            }
        } else if message == WM_STYLECHANGING && wparam as i32 == GWL_STYLE {
            let style = unsafe { &mut *(lparam as *mut STYLESTRUCT) };
            style.styleNew &= !WS_VISIBLE;
        }
    }
    unsafe { DefSubclassProc(window, message, wparam, lparam) }
}

pub(crate) fn tray_action(event: &TrayIconEvent) -> Option<DesktopAction> {
    matches!(
        event,
        TrayIconEvent::DoubleClick {
            button: MouseButton::Left,
            ..
        }
    )
    .then_some(DesktopAction::Show)
}

pub(crate) fn menu_action(
    event: &MenuEvent,
    show: &MenuId,
    exit: &MenuId,
) -> Option<DesktopAction> {
    if event.id.eq(exit) {
        Some(DesktopAction::Exit)
    } else if event.id.eq(show) {
        Some(DesktopAction::Show)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tray_icon::{MouseButtonState, Rect, dpi::PhysicalPosition};

    #[test]
    fn only_left_double_click_opens_settings() {
        let event = |button| TrayIconEvent::DoubleClick {
            id: "test".into(),
            position: PhysicalPosition::new(0., 0.),
            rect: Rect::default(),
            button,
        };
        assert_eq!(
            tray_action(&event(MouseButton::Left)),
            Some(DesktopAction::Show)
        );
        assert_eq!(tray_action(&event(MouseButton::Right)), None);
        assert_eq!(tray_action(&event(MouseButton::Middle)), None);
        for button in [MouseButton::Left, MouseButton::Right] {
            for button_state in [MouseButtonState::Down, MouseButtonState::Up] {
                assert_eq!(
                    tray_action(&TrayIconEvent::Click {
                        id: "test".into(),
                        position: PhysicalPosition::new(0., 0.),
                        rect: Rect::default(),
                        button,
                        button_state,
                    }),
                    None
                );
            }
        }
    }

    #[test]
    fn menu_ids_select_only_the_requested_action() {
        let show = MenuId::new("show");
        let exit = MenuId::new("exit");
        assert_eq!(
            menu_action(&MenuEvent { id: show.clone() }, &show, &exit),
            Some(DesktopAction::Show)
        );
        assert_eq!(
            menu_action(&MenuEvent { id: exit.clone() }, &show, &exit),
            Some(DesktopAction::Exit)
        );
        assert_eq!(
            menu_action(
                &MenuEvent {
                    id: MenuId::new("other")
                },
                &show,
                &exit
            ),
            None
        );
    }

    #[test]
    fn exit_survives_a_later_activation_request() {
        let requests = DesktopRequests::default();
        let ctx = egui::Context::default();
        requests.send(&ctx, DesktopAction::Exit);
        requests.send(&ctx, DesktopAction::Show);
        assert_eq!(requests.take(), Some(DesktopAction::Exit));
        assert_eq!(requests.take(), None);
    }
}
