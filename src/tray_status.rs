//! Notification-area status updates independent of the settings-window paint loop.
//!
//! All icon access, callbacks and timer cleanup stay on the creating GUI thread.

use anyhow::{Context, Result};
use std::{
    cell::RefCell,
    rc::Rc,
    time::{Duration, Instant},
};
use tray_icon::{Icon, TrayIcon};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TrayState {
    Idle,
    Preparing,
    Recording,
    Preview,
    Finalizing,
    Recovering,
    Failed,
}

impl TrayState {
    pub(crate) const fn tooltip(self) -> &'static str {
        match self {
            Self::Idle => "TalkRecorder — 待機中",
            Self::Preparing => "TalkRecorder — 録音準備中",
            Self::Recording => "TalkRecorder — 録音中",
            Self::Preview => "TalkRecorder — 入力確認中（保存なし）",
            Self::Finalizing => "TalkRecorder — 停止・保存中",
            Self::Recovering => "TalkRecorder — 復旧・圧縮中",
            Self::Failed => "TalkRecorder — エラーで停止（設定画面を確認）",
        }
    }

    const fn color(self) -> [u8; 4] {
        match self {
            Self::Idle => [56, 132, 232, 255],
            Self::Recording => [224, 61, 80, 255],
            Self::Preview => [40, 161, 196, 255],
            Self::Preparing | Self::Finalizing | Self::Recovering => [240, 155, 42, 255],
            Self::Failed => [235, 180, 32, 255],
        }
    }

    fn rgba(self) -> Vec<u8> {
        let mut pixels = vec![0; 32 * 32 * 4];
        for y in 0..32 {
            for x in 0..32 {
                let dx = x as i32 - 16;
                let dy = y as i32 - 16;
                let radius = dx * dx + dy * dy;
                let lit = match self {
                    Self::Idle => (80..=169).contains(&radius),
                    Self::Recording => radius <= 121,
                    Self::Preview => {
                        let half_height = match x {
                            7..=9 | 23..=25 => 4,
                            11..=13 | 19..=21 => 8,
                            15..=17 => 12,
                            _ => 0,
                        };
                        half_height > 0 && dy.abs() <= half_height
                    }
                    Self::Preparing => dx.abs() + dy.abs() <= 13 && radius >= 25,
                    Self::Finalizing => dx.abs() <= 10 && dy.abs() <= 10,
                    Self::Recovering => {
                        (80..=169).contains(&radius) && !(dx > 3 && dy < -3)
                            || (7..=13).contains(&dx) && (-9..=-3).contains(&dy)
                    }
                    Self::Failed => {
                        dx.abs() + dy.abs() <= 14
                            && !(dx.abs() <= 1 && ((-7..=2).contains(&dy) || (6..=8).contains(&dy)))
                    }
                };
                if lit {
                    let offset = (y * 32 + x) * 4;
                    pixels[offset..offset + 4].copy_from_slice(&self.color());
                }
            }
        }
        pixels
    }

    pub(crate) fn icon(self) -> Result<Icon> {
        Icon::from_rgba(self.rgba(), 32, 32).context("通知領域アイコンを作成できません")
    }
}

type StateSource = Box<dyn Fn() -> TrayState>;
type Renderer = Box<dyn Fn(TrayState) -> Result<()>>;

struct Controller {
    desired: TrayState,
    applied: Option<TrayState>,
    source: Option<StateSource>,
    render: Renderer,
    retry_after: Option<Instant>,
    error: Option<String>,
    last_reported_error: Option<String>,
}

impl Controller {
    fn new(render: impl Fn(TrayState) -> Result<()> + 'static) -> Self {
        Self {
            desired: TrayState::Idle,
            applied: None,
            source: None,
            render: Box::new(render),
            retry_after: None,
            error: None,
            last_reported_error: None,
        }
    }

    fn record_error(&mut self, message: String) {
        if self.last_reported_error.as_ref() != Some(&message) {
            eprintln!("{message}");
            self.error = Some(message.clone());
            self.last_reported_error = Some(message);
        }
    }

    fn poll(&mut self) -> Result<()> {
        let next = self
            .source
            .as_ref()
            .map(|source| std::panic::catch_unwind(std::panic::AssertUnwindSafe(source)));
        let next = match next {
            Some(Ok(next)) => next,
            Some(Err(_)) => {
                self.record_error("通知領域の状態取得でエラーが発生しました".into());
                TrayState::Failed
            }
            None => self.desired,
        };
        if self.desired != next {
            self.desired = next;
            self.retry_after = None;
        }
        if self.applied == Some(next)
            || self
                .retry_after
                .is_some_and(|deadline| Instant::now() < deadline)
        {
            return Ok(());
        }
        let rendered =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| (self.render)(next)))
                .unwrap_or_else(|_| Err(anyhow::anyhow!("通知領域の描画処理が異常終了しました")));
        match rendered {
            Ok(()) => {
                self.applied = Some(next);
                self.retry_after = None;
                Ok(())
            }
            Err(error) => {
                self.retry_after = Some(Instant::now() + Duration::from_secs(2));
                self.record_error(format!("通知領域の状態を更新できません: {error:#}"));
                Err(error)
            }
        }
    }

    fn set_state(&mut self, state: TrayState) {
        self.source = None;
        if self.desired != state {
            self.retry_after = None;
        }
        self.desired = state;
        let _ = self.poll();
    }

    fn watch(&mut self, source: impl Fn() -> TrayState + 'static) {
        self.source = Some(Box::new(source));
        let _ = self.poll();
    }
}

/// Own on the same GUI thread as `TrayIcon`; drop before removing the tray icon.
pub(crate) struct TrayMonitor {
    // Field order stops the timer before releasing the controller's TrayIcon.
    #[cfg(windows)]
    _timer: native::Timer,
    controller: Rc<RefCell<Controller>>,
}

impl TrayMonitor {
    pub(crate) fn new(tray: TrayIcon) -> Result<Self> {
        let controller = Rc::new(RefCell::new(Controller::new(move |state: TrayState| {
            tray.set_icon(Some(state.icon()?))
                .context("通知領域の色を変更できません")?;
            tray.set_tooltip(Some(state.tooltip()))
                .context("通知領域の説明を変更できません")?;
            Ok(())
        })));
        controller.borrow_mut().poll()?;
        #[cfg(windows)]
        let timer = native::Timer::new(Rc::downgrade(&controller))?;
        Ok(Self {
            #[cfg(windows)]
            _timer: timer,
            controller,
        })
    }

    /// Stop watching the previous source and immediately apply a manual state.
    pub(crate) fn set_state(&self, state: TrayState) {
        self.controller.borrow_mut().set_state(state);
    }

    /// Replace the state source. It is called only on this GUI thread.
    pub(crate) fn watch(&self, source: impl Fn() -> TrayState + 'static) {
        self.controller.borrow_mut().watch(source);
    }

    /// Surface a previous failure when the settings window is next updated.
    pub(crate) fn take_error(&self) -> Option<String> {
        self.controller.borrow_mut().error.take()
    }

    /// Other platforms have no Windows message timer and must poll in their UI.
    #[cfg(not(windows))]
    pub(crate) fn poll(&self) {
        let _ = self.controller.borrow_mut().poll();
    }
}

#[cfg(windows)]
mod native {
    use super::Controller;
    use anyhow::{Result, ensure};
    use std::{
        cell::RefCell,
        collections::HashMap,
        marker::PhantomData,
        rc::{Rc, Weak},
    };
    use windows::Win32::{
        Foundation::HWND,
        UI::WindowsAndMessaging::{KillTimer, SetTimer},
    };

    thread_local! {
        static CONTROLLERS: RefCell<HashMap<usize, Weak<RefCell<Controller>>>> = RefCell::new(HashMap::new());
    }

    pub(super) struct Timer {
        id: usize,
        _gui_thread: PhantomData<Rc<()>>,
    }

    impl Timer {
        pub(super) fn new(controller: Weak<RefCell<Controller>>) -> Result<Self> {
            // A NULL-HWND timer gets a system-allocated ID. It cannot collide
            // with tray-icon's private timers bound to its receiver HWND.
            let id = unsafe { SetTimer(None, 0, 250, Some(tick)) };
            ensure!(id != 0, "通知領域の状態監視タイマーを開始できません");
            CONTROLLERS.with(|entries| entries.borrow_mut().insert(id, controller));
            Ok(Self {
                id,
                _gui_thread: PhantomData,
            })
        }
    }

    unsafe extern "system" fn tick(_window: HWND, _message: u32, id: usize, _time: u32) {
        // Never let a Rust panic cross the Win32 callback boundary. Take a weak
        // reference before calling user code, so nested messages do not borrow
        // the thread-local map while they are dispatched.
        let _ = std::panic::catch_unwind(|| {
            let controller =
                CONTROLLERS.with(|entries| entries.borrow().get(&id).and_then(Weak::upgrade));
            if let Some(controller) = controller
                && let Ok(mut controller) = controller.try_borrow_mut()
            {
                let _ = controller.poll();
            }
        });
    }

    impl Drop for Timer {
        fn drop(&mut self) {
            // Already-queued callbacks become no-ops before the timer is killed.
            let _ = CONTROLLERS.try_with(|entries| entries.borrow_mut().remove(&self.id));
            if let Err(error) = unsafe { KillTimer(None, self.id) } {
                eprintln!("通知領域の状態監視タイマーを停止できません: {error}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    const STATES: [TrayState; 7] = [
        TrayState::Idle,
        TrayState::Preparing,
        TrayState::Recording,
        TrayState::Preview,
        TrayState::Finalizing,
        TrayState::Recovering,
        TrayState::Failed,
    ];

    #[test]
    fn states_have_distinct_shapes_and_japanese_tooltips_and_only_recording_is_red() {
        for (index, state) in STATES.into_iter().enumerate() {
            let pixels = state.rgba();
            assert_eq!(pixels.len(), 32 * 32 * 4);
            assert!(pixels.chunks_exact(4).any(|pixel| pixel[3] == 255));
            assert!(pixels.chunks_exact(4).any(|pixel| pixel[3] == 0));
            assert!(state.tooltip().contains("TalkRecorder"));
            assert!(!state.tooltip().is_ascii());
            let [red, green, blue, _] = state.color();
            assert_eq!(
                red > green && red > blue && green < 100,
                state == TrayState::Recording
            );
            for other in STATES.iter().skip(index + 1) {
                assert_ne!(pixels, other.rgba());
                assert_ne!(state.tooltip(), other.tooltip());
            }
        }
    }

    #[test]
    fn watched_state_changes_update_once_and_manual_state_detaches_the_source() {
        let observed = Rc::new(RefCell::new(Vec::new()));
        let rendered = observed.clone();
        let mut controller = Controller::new(move |state| {
            rendered.borrow_mut().push(state);
            Ok(())
        });
        let phase = Rc::new(Cell::new(TrayState::Preparing));
        let source = phase.clone();
        controller.watch(move || source.get());
        controller.poll().unwrap();
        phase.set(TrayState::Recording);
        controller.poll().unwrap();
        controller.poll().unwrap();
        controller.set_state(TrayState::Idle);
        phase.set(TrayState::Failed);
        controller.poll().unwrap();
        assert_eq!(
            *observed.borrow(),
            [TrayState::Preparing, TrayState::Recording, TrayState::Idle]
        );
    }

    #[test]
    fn failed_update_is_reported_and_does_not_render_on_every_tick() {
        let attempts = Rc::new(Cell::new(0));
        let calls = attempts.clone();
        let mut controller = Controller::new(move |_| {
            calls.set(calls.get() + 1);
            anyhow::bail!("fixture render failure")
        });
        assert!(controller.poll().is_err());
        let error = controller.error.take().unwrap();
        assert!(error.contains("fixture render failure"));
        controller.poll().unwrap();
        assert_eq!(attempts.get(), 1);
        controller.set_state(TrayState::Recording);
        assert_eq!(attempts.get(), 2);
    }

    #[test]
    fn panicking_source_is_reported_as_failed_without_unwinding_into_timer() {
        let observed = Rc::new(RefCell::new(Vec::new()));
        let rendered = observed.clone();
        let mut controller = Controller::new(move |state| {
            rendered.borrow_mut().push(state);
            Ok(())
        });
        controller.watch(|| panic!("fixture state source panic"));
        assert_eq!(*observed.borrow(), [TrayState::Failed]);
        assert!(controller.error.take().unwrap().contains("状態取得"));
        controller.watch(|| TrayState::Idle);
        assert_eq!(*observed.borrow(), [TrayState::Failed, TrayState::Idle]);
    }

    #[cfg(windows)]
    #[test]
    fn native_timer_polls_without_any_visible_window_and_stops_on_drop() {
        use windows::Win32::UI::WindowsAndMessaging::{
            DispatchMessageW, MSG, PM_REMOVE, PeekMessageW, WM_TIMER,
        };
        let observed = Rc::new(RefCell::new(Vec::new()));
        let rendered = observed.clone();
        let controller = Rc::new(RefCell::new(Controller::new(move |state| {
            rendered.borrow_mut().push(state);
            Ok(())
        })));
        controller.borrow_mut().watch(|| TrayState::Recording);
        // Reset just the renderer cache to observe the actual WM_TIMER callback.
        controller.borrow_mut().applied = None;
        observed.borrow_mut().clear();
        let timer = native::Timer::new(Rc::downgrade(&controller)).unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while observed.borrow().is_empty() && Instant::now() < deadline {
            let mut message = MSG::default();
            while unsafe { PeekMessageW(&mut message, None, WM_TIMER, WM_TIMER, PM_REMOVE) }
                .as_bool()
            {
                let _ = unsafe { DispatchMessageW(&message) };
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(*observed.borrow(), [TrayState::Recording]);
        drop(timer);
        controller.borrow_mut().applied = None;
        observed.borrow_mut().clear();
        // Deliver any timer message already queued when KillTimer was called.
        let mut message = MSG::default();
        while unsafe { PeekMessageW(&mut message, None, WM_TIMER, WM_TIMER, PM_REMOVE) }.as_bool() {
            let _ = unsafe { DispatchMessageW(&message) };
        }
        assert!(observed.borrow().is_empty());
    }
}
