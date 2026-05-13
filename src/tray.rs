use tray_icon::{
    menu::{Menu, MenuId, MenuItem, PredefinedMenuItem},
    Icon, TrayIcon, TrayIconBuilder,
};

pub struct AppTray {
    _icon: TrayIcon,
    pub show_item_id: MenuId,
    pub scan_item_id: MenuId,
    pub quit_item_id: MenuId,
}

impl AppTray {
    pub fn build() -> Option<Self> {
        let bytes = include_bytes!("../assets/icon.png");
        let img = image::load_from_memory(bytes).ok()?.to_rgba8();
        let (w, h) = img.dimensions();
        let icon = Icon::from_rgba(img.into_raw(), w, h).ok()?;

        let show_item = MenuItem::new("Show / Hide", true, None);
        let scan_item = MenuItem::new("Scan Now", true, None);
        let quit_item = MenuItem::new("Quit", true, None);

        let show_id = show_item.id().clone();
        let scan_id = scan_item.id().clone();
        let quit_id = quit_item.id().clone();

        let menu = Menu::new();
        menu.append_items(&[
            &show_item,
            &PredefinedMenuItem::separator(),
            &scan_item,
            &PredefinedMenuItem::separator(),
            &quit_item,
        ])
        .ok()?;

        let tray = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_icon(icon)
            .with_tooltip("Disk Cleaner & Duplicate Finder")
            .build()
            .ok()?;

        Some(AppTray {
            _icon: tray,
            show_item_id: show_id,
            scan_item_id: scan_id,
            quit_item_id: quit_id,
        })
    }
}
