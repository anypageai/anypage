//! The gallery page: loads, draws both themes without script errors, and
//! looks right numerically.

mod common;

use anypage_engine::Page;
use anypage_runtime::{Data, ScriptPage};
use common::*;

fn gallery() -> ScriptPage {
    let pkg = anypage_ui::gallery().expect("gallery package");
    ScriptPage::load(&pkg, &natives(), text(), wake()).unwrap_or_else(|e| panic!("gallery: {e}"))
}

#[test]
fn gallery_loads_and_draws_both_themes() {
    let mut p = gallery();
    assert_eq!(p.spec().panels.len(), 2);
    for panel in [0, 1] {
        let cmds = display(&mut p, panel);
        let n = cmds.items().len();
        assert!(n > 200, "panel {panel} drew only {n} commands");
        for s in ["AnyPage UI", "Buttons", "Primary", "Toggles", "Wi-Fi", "Row 1", "Taipei"] {
            assert!(has_text_containing(&cmds, s), "panel {panel} lacks {s:?}: {:?}", &texts(&cmds)[..20.min(texts(&cmds).len())]);
        }
    }
    assert_eq!(p.handler_errors, 0);
    let _ = Data::Nil;
}

#[test]
fn gallery_pixels() {
    let mut p = gallery();
    let dark = raster(&mut p, 0);
    let light = raster(&mut p, 1);
    save_png("gallery-dark.png", &dark);
    save_png("gallery-light.png", &light);
    assert_eq!((dark.w, dark.h), (1180, 1500));
    // backgrounds: the themes' bg tokens at a corner
    assert_eq!(pixel(&dark, 2, 2), [16, 18, 23, 255]);
    assert_eq!(pixel(&light, 2, 2), [244, 245, 248, 255]);
    // the primary button is filled with each theme's accent (sampled inside
    // the button, left of its label)
    for (app, img, accent) in [("dark", &dark, [92, 152, 255, 255]), ("light", &light, [36, 104, 228, 255])] {
        let b = node(&describe(&mut p, app), "primary");
        let ox = if app == "light" { 2400.0 - 1180.0 } else { 0.0 };
        let (x, y) = ((b.get("x").num_or(0.0) - ox + 5.0) as u32, (b.get("y").num_or(0.0) + b.get("h").num_or(0.0) / 2.0) as u32);
        assert_eq!(pixel(img, x, y), accent, "{app} primary button at {x},{y}");
        // card surfaces differ from the page background
        let card = pixel(img, x - 20, y);
        assert_ne!(card, pixel(img, 2, 2), "{app} card surface");
    }
    // coverage: a good part of each panel is not background (cards, text)
    for img in [&dark, &light] {
        let bg = pixel(img, 2, 2);
        let differs = img.rgba.as_chunks::<4>().0.iter().filter(|p| **p != bg).count();
        let frac = differs as f64 / (img.w * img.h) as f64;
        assert!(frac > 0.35, "only {frac:.2} of the panel is drawn");
    }
}

#[test]
fn gallery_interactions_stay_in_their_app() {
    let mut p = gallery();
    click_id(&mut p, "dark", "primary");
    click_id(&mut p, "dark", "primary");
    assert_eq!(state(&mut p, "dark").get("count").num(), Some(2.0));
    assert_eq!(state(&mut p, "light").get("count").num(), Some(0.0));
    assert!(has_text(&display(&mut p, 0), "Clicked 2 times"));
    assert!(has_text(&display(&mut p, 1), "Clicked 0 times"));
    // type into the light app's name field, submit, toast
    click_id(&mut p, "light", "name");
    for c in ["A", "n", "y"] {
        p.text_input(c);
    }
    assert!(p.key("enter"));
    assert_eq!(state(&mut p, "light").get("name").as_str(), Some("Any"));
    assert!(has_text(&display(&mut p, 1), "Hello, Any"));
    // open the select menu in the dark app and save the picture
    click_id(&mut p, "dark", "fruit");
    assert!(has_text(&display(&mut p, 0), "Elderberry"));
    let img = raster(&mut p, 0);
    save_png("gallery-dark-menu.png", &img);
    assert!(p.key("escape"));
    // the dialog in the light app
    click_id(&mut p, "light", "open-dialog");
    let img = raster(&mut p, 1);
    save_png("gallery-light-dialog.png", &img);
    // the scrim darkens the light background
    let bg = pixel(&img, 2, 2);
    assert!(bg[0] < 220, "scrim not drawn: {bg:?}");
    click_id(&mut p, "light", "delete");
    assert_eq!(state(&mut p, "light").get("dialog").as_bool(), Some(false));
    // keyboard focus ring in the dark app: Tab from the name field
    click_id(&mut p, "dark", "name");
    assert!(p.key("tab"));
    assert_eq!(ui_state(&mut p, "dark").get("focus").as_str(), Some("password"));
    // the list and the table work inside the scrolled body
    click_id(&mut p, "dark", "list#3");
    assert_eq!(state(&mut p, "dark").get("row").num(), Some(3.0));
    assert_eq!(p.handler_errors, 0);
    let img = raster(&mut p, 0);
    save_png("gallery-dark-used.png", &img);
}
