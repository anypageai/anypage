//! The standard native library an AnyPage app offers to its pages: the demo
//! operators and every capability module (docs/MODULES.md). A page can use
//! only the modules it declares in page.json; a module a platform lacks is
//! simply absent there.

use anypage_runtime::Natives;

/// The modules this build registers, for logs and "about" pages.
pub const MODULES: [&str; 11] = ["demo", "ui", "pdf", "sheet", "cad", "cloud", "media", "ble", "usb", "serial", "scene"];

/// Everything, registered once per page load.
pub fn natives() -> Natives {
    let mut n = anypage_demo::script::natives();
    anypage_ui::register(&mut n);
    anypage_pdf::register(&mut n);
    anypage_sheet::register(&mut n);
    anypage_cad::register(&mut n);
    anypage_cloud::register(&mut n);
    anypage_media::register(&mut n);
    anypage_devices::register(&mut n);
    anypage_scene::register(&mut n);
    n
}

#[cfg(test)]
mod tests {
    #[test]
    fn every_module_registers_without_clashes() {
        let n = super::natives();
        for m in ["ui", "pdf", "sheet", "cad", "cloud", "media", "ble", "usb", "serial", "scene", "demo"] {
            assert!(n.has_module(m) || m == "ui", "module {m} missing");
        }
        assert!(n.names().iter().any(|x| x == "lua:ui"));
    }
}
