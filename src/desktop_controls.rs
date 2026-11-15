use eframe::egui;
use std::sync::atomic::{AtomicU8, Ordering};
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
}

impl DesktopRequests {
    pub(crate) fn new(cc: &eframe::CreationContext<'_>) -> anyhow::Result<Self> {
        #[cfg(windows)]
        {
            use raw_window_handle::{HasWindowHandle, RawWindowHandle};
            let RawWindowHandle::Win32(handle) = cc.window_handle()?.as_raw() else {
                anyhow::bail!("設定画面のWindowsハンドルを取得できません");
            };
            Ok(Self {
                window: handle.hwnd.get() as usize,
                ..Self::default()
            })
        }
        #[cfg(not(windows))]
        {
            let _ = cc;
            Ok(Self::default())
        }
    }

    pub(crate) fn send(&self, ctx: &egui::Context, action: DesktopAction) {
        // 終了と表示が競合しても、終了要求を上書きしない。
        self.pending.fetch_max(action as u8, Ordering::AcqRel);
        // 非表示ウィンドウでは描画が止まるため、updateより先に表示を要求する。
        #[cfg(windows)]
        if self.window != 0 {
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
