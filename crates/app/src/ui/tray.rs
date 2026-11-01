//! Tray (Windows notification area / macOS menu bar) icon and menu.
//!
//! Menu and icon events arrive on the UI thread's message loop; they are forwarded as [`Action`]s
//! and a repaint is requested so `App::logic` handles them even while the window is hidden.

use crossbeam_channel::Sender;
use eframe::egui;
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

use super::format::tray_status_line;
use crate::icon::icon_rgba;
use soundsync_engine::Status;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Show,
    ToggleBroadcast,
    ToggleMute,
    ToggleSyncTest,
    InstallUpdate,
    Quit,
}

/// The update item comes and goes, so it has a fixed id rather than one captured at start.
const UPDATE_ID: &str = "soundsync-install-update";

pub struct Tray {
    icon: TrayIcon,
    status: MenuItem,
    broadcast: Option<CheckMenuItem>,
    mute: CheckMenuItem,
    sync: CheckMenuItem,
    last_line: String,
    menu: Menu,
    /// "Update to build N", while there is one to install.
    update: Option<(u32, MenuItem)>,
}

impl Tray {
    /// Must run on the UI thread after the event loop started (eframe's creator closure).
    pub fn create(ctx: egui::Context, tx: Sender<Action>, can_broadcast: bool) -> anyhow::Result<Tray> {
        let status = MenuItem::new("Starting…", false, None);
        let open = MenuItem::new("Open SoundSync", true, None);
        let broadcast = can_broadcast.then(|| CheckMenuItem::new("Broadcast this PC", true, false, None));
        let mute = CheckMenuItem::new("Mute", true, false, None);
        let sync = CheckMenuItem::new("Sync test", true, false, None);
        let quit = MenuItem::new("Quit", true, None);
        let menu = Menu::new();
        menu.append(&status)?;
        menu.append(&PredefinedMenuItem::separator())?;
        menu.append(&open)?;
        if let Some(b) = &broadcast {
            menu.append(b)?;
        }
        menu.append(&mute)?;
        menu.append(&sync)?;
        menu.append(&PredefinedMenuItem::separator())?;
        menu.append(&quit)?;

        let ids = (
            open.id().clone(),
            broadcast.as_ref().map(|b| b.id().clone()),
            mute.id().clone(),
            sync.id().clone(),
            quit.id().clone(),
        );
        let (menu_tx, menu_ctx) = (tx.clone(), ctx.clone());
        MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
            let action = if e.id == ids.0 {
                Action::Show
            } else if Some(&e.id) == ids.1.as_ref() {
                Action::ToggleBroadcast
            } else if e.id == ids.2 {
                Action::ToggleMute
            } else if e.id == ids.3 {
                Action::ToggleSyncTest
            } else if e.id == ids.4 {
                Action::Quit
            } else if e.id == UPDATE_ID {
                Action::InstallUpdate
            } else {
                return;
            };
            let _ = menu_tx.send(action);
            menu_ctx.request_repaint();
        }));
        TrayIconEvent::set_event_handler(Some(move |e: TrayIconEvent| {
            let show = matches!(
                e,
                TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. }
                    | TrayIconEvent::DoubleClick { button: MouseButton::Left, .. }
            );
            if show {
                let _ = tx.send(Action::Show);
                ctx.request_repaint();
            }
        }));

        let icon = TrayIconBuilder::new()
            .with_menu(Box::new(menu.clone()))
            .with_menu_on_left_click(false)
            .with_tooltip("SoundSync")
            .with_icon(Icon::from_rgba(icon_rgba(32), 32, 32)?)
            .build()?;
        Ok(Tray { icon, status, broadcast, mute, sync, last_line: String::new(), menu, update: None })
    }

    pub fn update(&mut self, st: &Status) {
        let line = tray_status_line(st);
        if line != self.last_line {
            self.status.set_text(&line);
            let _ = self.icon.set_tooltip(Some(format!("SoundSync — {line}")));
            self.last_line = line;
        }
        if let Some(b) = &self.broadcast {
            b.set_checked(st.broadcasting);
        }
        self.mute.set_checked(st.muted);
        self.sync.set_checked(st.sync_test);
    }

    /// Shows "Update to build N" above the last separator while `build` is installable.
    pub fn set_update(&mut self, build: Option<u32>) {
        if self.update.as_ref().map(|(n, _)| *n) == build {
            return;
        }
        if let Some((_, item)) = self.update.take() {
            let _ = self.menu.remove(&item);
        }
        if let Some(n) = build {
            let item = MenuItem::with_id(MenuId::new(UPDATE_ID), format!("Update to build {n}"), true, None);
            let before_quit = self.menu.items().len().saturating_sub(2);
            if let Err(e) = self.menu.insert(&item, before_quit) {
                tracing::warn!("cannot add the update item to the tray menu: {e}");
            }
            self.update = Some((n, item));
        }
    }
}
