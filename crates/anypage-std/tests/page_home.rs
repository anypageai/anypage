//! The launcher page (`pages/home`): loads with every module and with
//! modules missing, draws both layouts, and opens pages by keyboard, by
//! pointer and through the agent action `open_page`; every way ends in
//! `PageRequest::Navigate(name)` from `take_request`. Also checks that its
//! catalog matches the `pages/` folder and prints view and raster timings.
//!
//! Needs `anypage-engine` as a dev-dependency of `anypage-std` (the `Page`
//! trait, `PageRequest`, `TextRaster`). A harness that runs this file from
//! elsewhere sets `ANYPAGE_ROOT` to the repository root.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use anypage_engine::core::{CellId, Rect, Status};
use anypage_engine::{Page, PageRequest, TextRaster};
use anypage_runtime::{Data, DisplayList, Manifest, Natives, Package, ScriptPage};

// ---------------------------------------------------------------- helpers

const WIDE: (f32, f32, f32, f32) = (0.0, 0.0, 1440.0, 900.0);
const NARROW: (f32, f32, f32, f32) = (2400.0, 0.0, 400.0, 820.0);

fn root() -> PathBuf {
    match std::env::var_os("ANYPAGE_ROOT") {
        Some(d) => PathBuf::from(d),
        None => PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..").join(".."),
    }
}

fn home_dir() -> PathBuf {
    root().join("pages").join("home")
}

/// One text system per page, as each engine has one (a shared one makes
/// layout slower under parallel tests for reasons a real page does not have).
fn text() -> Arc<TextRaster> {
    Arc::new(TextRaster::load().expect("a system font"))
}

fn load_with(natives: &Natives) -> ScriptPage {
    let pkg = Package::from_dir(&home_dir()).unwrap_or_else(|e| panic!("pages/home: {e}"));
    let wake: anypage_engine::Wake = Arc::new(|| {});
    ScriptPage::load(&pkg, natives, text(), wake).unwrap_or_else(|e| panic!("pages/home failed to load: {e}"))
}

fn load() -> ScriptPage {
    load_with(&anypage_std::natives())
}

/// The registry without some modules, as a platform or build that lacks them.
fn natives_without(skip: &[&str]) -> Natives {
    let mut n = Natives::new();
    if !skip.contains(&"demo") {
        n = anypage_demo::script::natives();
    }
    anypage_ui::register(&mut n);
    if !skip.contains(&"pdf") {
        anypage_pdf::register(&mut n);
    }
    if !skip.contains(&"sheet") {
        anypage_sheet::register(&mut n);
    }
    if !skip.contains(&"cad") {
        anypage_cad::register(&mut n);
    }
    if !skip.contains(&"cloud") {
        anypage_cloud::register(&mut n);
    }
    if !skip.contains(&"media") {
        anypage_media::register(&mut n);
    }
    if !skip.contains(&"devices") {
        anypage_devices::register(&mut n);
    }
    if !skip.contains(&"scene") {
        anypage_scene::register(&mut n);
    }
    n
}

fn act(p: &mut ScriptPage, name: &str, params: Data) -> Data {
    p.run_action(name, &params).unwrap_or_else(|e| panic!("action {name}: {e}"))
}

fn requests(p: &mut ScriptPage) -> Vec<PageRequest> {
    let mut out = Vec::new();
    while let Some(r) = p.take_request() {
        out.push(r);
    }
    out
}

fn navigations(p: &mut ScriptPage) -> Vec<String> {
    requests(p)
        .into_iter()
        .filter_map(|r| match r {
            PageRequest::Navigate(n) => Some(n),
            _ => None,
        })
        .collect()
}

/// The catalog as the page reports it: (name, entry) in display order.
fn pages(p: &mut ScriptPage) -> Vec<(String, Data)> {
    let l = act(p, "list_pages", Data::Nil);
    l.get("pages").items().iter().map(|e| (e.get("name").str_or("").to_string(), e.clone())).collect()
}

fn split(s: &str) -> BTreeSet<String> {
    s.split(',').filter(|x| !x.is_empty()).map(str::to_string).collect()
}

/// Every node of an app's UI (its describe action pages through 40 at a time).
fn describe(p: &mut ScriptPage, app: &str) -> Vec<Data> {
    let name = format!("{app}.describe");
    let mut nodes = Vec::new();
    let mut from = 1.0;
    for _ in 0..100 {
        let page = act(p, &name, Data::map([("from", Data::Num(from))]));
        nodes.extend(page.get("nodes").items().iter().cloned());
        match page.get("next").num() {
            Some(n) => from = n,
            None => return nodes,
        }
    }
    panic!("describe never ends");
}

fn node(nodes: &[Data], id: &str) -> Data {
    nodes.iter().find(|n| n.get("id").as_str() == Some(id)).cloned().unwrap_or_else(|| panic!("no node {id}"))
}

fn center(n: &Data) -> (f32, f32) {
    let x = n.get("x").num_or(0.0) + n.get("w").num_or(0.0) / 2.0;
    let y = n.get("y").num_or(0.0) + n.get("h").num_or(0.0) / 2.0;
    (x as f32, y as f32)
}

fn click(p: &mut ScriptPage, (x, y): (f32, f32)) {
    p.pointer_down(x, y);
    p.pointer_up();
}

fn focus(p: &mut ScriptPage, app: &str) -> Option<String> {
    let u = p.cell_value(&format!("{app}.ui")).expect("ui cell");
    u.get("focus").as_str().map(str::to_string)
}

fn rect(r: (f32, f32, f32, f32)) -> Rect {
    Rect::new(r.0, r.1, r.2, r.3)
}

/// The script cell (display list) of the panel whose rect starts at `x`.
fn panel_cells(p: &ScriptPage, x: f32) -> (CellId, Rect) {
    let panel = p.spec().panels.iter().find(|q| q.rect.x == x).expect("a panel at that x");
    (p.graph().inputs_of(panel.cell)[0], panel.rect)
}

fn display(p: &mut ScriptPage, x: f32) -> DisplayList {
    let (cell, _) = panel_cells(p, x);
    let i = p.parts().0.demand(cell);
    match &i.status {
        Status::Ready => {}
        other => panic!("panel at x={x} is not ready: {other:?}"),
    }
    i.get::<DisplayList>().expect("a display list").clone()
}

/// Every string drawn with "text" commands.
fn texts(list: &DisplayList) -> Vec<String> {
    list.cmds.items().iter().filter(|c| c.at(1).as_str() == Some("text")).filter_map(|c| c.at(6).as_str().map(str::to_string)).collect()
}

fn assert_no_error_card(p: &ScriptPage, list: &DisplayList, what: &str) {
    let t = texts(list);
    // the UI library's error card, and the runtime's panel error image
    assert!(!t.iter().any(|s| s.contains("the view failed") || s.contains("script error")), "{what}: the view shows an error: {t:?}");
    assert!(!list.cmds.items().is_empty(), "{what}: draws nothing");
    let _ = p;
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(f64::total_cmp);
    v[v.len() / 2]
}

// ---------------------------------------------------------------- tests

#[test]
fn loads_within_its_limits_and_draws_both_layouts() {
    let mut p = load();
    println!("{}", p.summary());
    assert!(p.load_ms < p.manifest.limits.load_ms, "load took {:.1} ms, limit {}", p.load_ms, p.manifest.limits.load_ms);
    assert_eq!(p.manifest.bounds, rect(WIDE), "bounds are the wide layout");
    assert_eq!(p.spec().panels.len(), 2, "one panel per layout");
    // nothing asked of the host at load on a desktop: the wide layout is the bounds
    assert!(requests(&mut p).is_empty());

    let list = pages(&mut p);
    assert!(list.len() >= 13, "13 pages or more, got {}", list.len());
    let names: Vec<&str> = list.iter().map(|(n, _)| n.as_str()).collect();
    for want in ["demo", "ui-gallery", "pdf-demo", "sheet-demo", "cad-demo", "cloud-demo", "live-demo", "devices-demo", "scene-demo", "text-input-test", "studio", "pdf-agent", "editor"] {
        assert!(names.contains(&want), "{want} is not in the launcher: {names:?}");
    }
    assert!(!names.contains(&"home"), "the launcher does not list itself");
    let all = act(&mut p, "list_pages", Data::Nil);
    assert_eq!(all.get("platform").as_str(), Some(std::env::consts::OS));
    assert_eq!(all.get("layout").as_str(), Some("wide"));
    // with every module registered, every page can be opened
    for (n, e) in &list {
        assert!(e.get("can_open").truthy(), "{n} cannot be opened with every module: {e:?}");
        assert_eq!(e.get("missing").as_str(), Some(""), "{n}");
    }

    for (x, app) in [(WIDE.0, "wide"), (NARROW.0, "narrow")] {
        let d = display(&mut p, x);
        assert_no_error_card(&p, &d, app);
        let t = texts(&d);
        assert!(t.iter().any(|s| s == "AnyPage"), "{app}: no title in {t:?}");
        let nodes = describe(&mut p, app);
        let (_, r) = panel_cells(&p, x);
        for (name, e) in &list {
            // cards scrolled out of view are not drawn
            let c = node(&nodes, &format!("card:{name}"));
            if c.get("y").num_or(0.0) + c.get("h").num_or(0.0) > r.h as f64 {
                continue;
            }
            for key in ["title", "en"] {
                let s = e.get(key).str_or("");
                assert!(t.iter().any(|x| x == s), "{app}: {key} {s:?} of {name} not drawn");
            }
        }
        assert!(t.iter().any(|s| s.contains(match std::env::consts::OS {
            "windows" => "Windows",
            "linux" => "Linux",
            "macos" => "macOS",
            other => other,
        })), "{app}: the platform is not shown");
        // the about text: local features free and open source, cloud optional
        assert!(t.iter().any(|s| s.contains("免費") && s.contains("開源")), "{app}: no about text");
        assert_eq!(p.handler_errors, 0, "{app}: handler errors");
        let cards: Vec<&Data> = nodes.iter().filter(|n| n.get("type").as_str() == Some("launch_card")).collect();
        assert_eq!(cards.len(), list.len(), "{app}: one card per page");
        for c in &cards {
            let page = c.get("page").str_or("");
            assert_eq!(c.get("id").as_str(), Some(format!("card:{page}").as_str()));
            assert!(c.get("can_open").truthy());
        }
        let clips = d.cmds.items().iter().filter(|c| c.at(1).as_str() == Some("clip")).count();
        println!("{app}: {} display commands ({clips} clips), {} nodes, {} cards", d.cmds.items().len(), nodes.len(), cards.len());
    }
    // the wide layout's cards and side column fit its rect: it needs no
    // scroll container (whose clip doubles the raster time)
    let nodes = describe(&mut p, "wide");
    let d = display(&mut p, WIDE.0);
    let clips = d.cmds.items().iter().filter(|c| c.at(1).as_str() == Some("clip")).count();
    assert_eq!(clips, 0, "the wide layout clips");
    for n in nodes.iter().filter(|n| n.get("type").as_str() == Some("launch_card") || n.get("id").as_str() == Some("side")) {
        let bottom = n.get("y").num_or(0.0) + n.get("h").num_or(0.0);
        assert!(bottom <= WIDE.3 as f64 - 20.0, "{:?} ends at {bottom}, below the wide layout", n.get("id"));
    }
}

#[test]
fn keyboard_moves_between_cards_and_enter_opens() {
    let mut p = load();
    let list = pages(&mut p);
    let name = |i: usize| list[i].0.clone();
    let card = |i: usize| Some(format!("card:{}", list[i].0));
    let n = list.len();

    // keys the page does not use fall through (the host's own keys, "h" for its HUD)
    assert!(!p.key("x"));
    assert!(!p.key("h"));
    // nothing focused: the first arrow focuses the first card
    assert!(p.key("right"));
    assert_eq!(focus(&mut p, "wide"), card(0));
    assert!(p.key("right"));
    assert_eq!(focus(&mut p, "wide"), card(1));
    assert!(p.key("down"));
    assert_eq!(focus(&mut p, "wide"), card(4), "down moves one row of three");
    assert!(p.key("left"));
    assert_eq!(focus(&mut p, "wide"), card(3));
    assert!(p.key("up"));
    assert_eq!(focus(&mut p, "wide"), card(0));
    assert!(p.key("up"));
    assert_eq!(focus(&mut p, "wide"), card(0), "up from the first row stays");
    assert!(p.key("left"));
    assert_eq!(focus(&mut p, "wide"), card(0));
    assert!(p.key("end"));
    assert_eq!(focus(&mut p, "wide"), card(n - 1));
    assert!(p.key("right"));
    assert_eq!(focus(&mut p, "wide"), card(n - 1));
    assert!(p.key("home"));
    assert_eq!(focus(&mut p, "wide"), card(0));
    assert!(requests(&mut p).is_empty(), "moving the focus opens nothing");

    // Enter opens the focused card
    assert!(p.key("right"));
    assert!(p.key("enter"));
    assert_eq!(navigations(&mut p), vec![name(1)]);
    // so does Space
    assert!(p.key("down"));
    assert!(p.key("space"));
    assert_eq!(navigations(&mut p), vec![name(4)]);
    let st = p.cell_value("wide.state").expect("state");
    assert_eq!(st.get("opened").as_str(), Some(name(4).as_str()));

    // with nothing focused, the first Enter only shows the focus
    let mut q = load();
    assert!(q.key("enter"));
    assert_eq!(focus(&mut q, "wide"), card(0));
    assert!(navigations(&mut q).is_empty());
    assert!(q.key("enter"));
    assert_eq!(navigations(&mut q), vec![name(0)]);
    // Escape drops the focus (the library); a second one is the host's
    assert!(q.key("escape"));
    assert_eq!(focus(&mut q, "wide"), None);
    assert!(!q.key("escape"));

    // R fits the current layout instead of the whole bounds
    assert!(p.key("r"));
    assert_eq!(requests(&mut p), vec![PageRequest::FitRect(rect(WIDE))]);
    assert_eq!(p.handler_errors, 0);
}

#[test]
fn tapping_a_card_opens_its_page() {
    let mut p = load();
    let nodes = describe(&mut p, "wide");
    for name in ["pdf-demo", "devices-demo", "text-input-test"] {
        let c = node(&nodes, &format!("card:{name}"));
        // anywhere on the card: its centre, and near its top-left corner (the icon)
        click(&mut p, center(&c));
        assert_eq!(navigations(&mut p), vec![name.to_string()], "centre of {name}");
        let corner = (c.get("x").num_or(0.0) as f32 + 24.0, c.get("y").num_or(0.0) as f32 + 24.0);
        click(&mut p, corner);
        assert_eq!(navigations(&mut p), vec![name.to_string()], "icon of {name}");
    }
    // a press released outside the card opens nothing
    let c = node(&nodes, "card:sheet-demo");
    let (x, y) = center(&c);
    p.pointer_down(x, y);
    p.pointer_drag(x, y + 400.0);
    p.pointer_up();
    assert!(navigations(&mut p).is_empty());
    // a click between cards opens nothing either
    click(&mut p, (WIDE.2 - 4.0, 4.0));
    assert!(navigations(&mut p).is_empty());
    assert_eq!(p.handler_errors, 0);
}

#[test]
fn narrow_layout_scrolls_by_dragging_and_opens_by_tapping() {
    let mut p = load();
    let r = act(&mut p, "set_layout", Data::map([("layout", Data::str("narrow"))]));
    assert_eq!(r.get("layout").as_str(), Some("narrow"));
    assert_eq!(requests(&mut p), vec![PageRequest::FitRect(rect(NARROW))]);
    // the narrow layout fills the 400 x 820 rect, one card per row
    let nodes = describe(&mut p, "narrow");
    let cards: Vec<&Data> = nodes.iter().filter(|n| n.get("type").as_str() == Some("launch_card")).collect();
    let x0 = cards[0].get("x").num_or(0.0);
    assert!(cards.iter().all(|c| (c.get("x").num_or(0.0) - x0).abs() < 0.5), "one column");
    assert!(cards.iter().all(|c| c.get("x").num_or(0.0) >= NARROW.0 as f64 && c.get("w").num_or(0.0) > 340.0));

    // keys go to the narrow app now; down moves one card
    let list = pages(&mut p);
    assert!(p.key("down"));
    assert_eq!(focus(&mut p, "narrow"), Some(format!("card:{}", list[0].0)));
    assert!(p.key("down"));
    assert_eq!(focus(&mut p, "narrow"), Some(format!("card:{}", list[1].0)));
    // the last card is below the fold: focusing it scrolls the list
    assert!(p.key("end"));
    let u = p.cell_value("narrow.ui").expect("ui");
    let scrolled = u.get("scroll").get("pages").num_or(0.0);
    assert!(scrolled > 100.0, "End scrolls the list, got {scrolled}");
    assert!(p.key("home"));

    // a press that moves is a scroll, not a tap
    let nodes = describe(&mut p, "narrow");
    let c = node(&nodes, &format!("card:{}", list[3].0));
    let (x, y) = center(&c);
    let before = p.cell_value("narrow.ui").expect("ui").get("scroll").get("pages").num_or(0.0);
    p.pointer_down(x, y);
    p.pointer_drag(x, y - 60.0);
    p.pointer_drag(x, y - 240.0);
    p.pointer_up();
    assert!(navigations(&mut p).is_empty(), "a drag opened a page");
    let after = p.cell_value("narrow.ui").expect("ui").get("scroll").get("pages").num_or(0.0);
    assert!(after > before + 100.0, "dragging scrolls: {before} -> {after}");

    // a tap opens
    let nodes = describe(&mut p, "narrow");
    let c = node(&nodes, &format!("card:{}", list[5].0));
    click(&mut p, center(&c));
    assert_eq!(navigations(&mut p), vec![list[5].0.clone()]);

    // the header button goes back to the wide layout
    let b = node(&nodes, "to-wide");
    click(&mut p, center(&b));
    assert_eq!(requests(&mut p), vec![PageRequest::FitRect(rect(WIDE))]);
    assert_eq!(act(&mut p, "list_pages", Data::Nil).get("layout").as_str(), Some("wide"));
    let d = display(&mut p, NARROW.0);
    assert_no_error_card(&p, &d, "narrow");
    assert!(act_err(&mut p, "set_layout", Data::map([("layout", Data::str("tall"))])).contains("wide"));
}

/// On Android and iOS the page starts with the narrow layout and asks the
/// host to fit it. The platform comes from the runtime, so this loads the
/// package with the line that reads it patched.
#[test]
fn mobile_platform_starts_with_the_narrow_layout() {
    let mut pkg = Package::from_dir(&home_dir()).expect("pages/home");
    let main = pkg.files.get("main.luau").expect("main.luau").clone();
    let anchor = "local PLATFORM = tostring(";
    assert!(main.contains(anchor), "main.luau no longer has {anchor:?}; update this test");
    pkg.files.insert("main.luau".into(), main.replacen(anchor, "local PLATFORM = \"android\" or tostring(", 1));
    let wake: anypage_engine::Wake = Arc::new(|| {});
    let mut p = ScriptPage::load(&pkg, &anypage_std::natives(), text(), wake).expect("loads");
    assert_eq!(requests(&mut p), vec![PageRequest::FitRect(rect(NARROW))], "fits the narrow layout at load");
    let l = act(&mut p, "list_pages", Data::Nil);
    assert_eq!(l.get("platform").as_str(), Some("android"));
    assert_eq!(l.get("layout").as_str(), Some("narrow"));
    let d = display(&mut p, NARROW.0);
    assert_no_error_card(&p, &d, "narrow on android");
    assert!(texts(&d).iter().any(|s| s == "Android"), "the platform badge");
    // Tab with nothing focused goes to the layout that is shown
    let list = pages(&mut p);
    assert!(p.key("tab"));
    assert_eq!(focus(&mut p, "narrow"), Some(format!("card:{}", list[0].0)));
    assert_eq!(focus(&mut p, "wide"), None);
    assert!(p.key("tab"), "the library moves on from there");
    assert_eq!(focus(&mut p, "narrow"), Some(format!("card:{}", list[1].0)));
    assert!(p.key("escape"));
    assert!(p.key("shift+tab"));
    assert_eq!(focus(&mut p, "narrow"), Some(format!("card:{}", list[list.len() - 1].0)));
    assert!(p.key("enter"));
    assert_eq!(navigations(&mut p), vec![list[list.len() - 1].0.clone()]);
    assert_eq!(p.handler_errors, 0);
}

fn act_err(p: &mut ScriptPage, name: &str, params: Data) -> String {
    match p.run_action(name, &params) {
        Ok(v) => panic!("action {name} should fail, gave {v:?}"),
        Err(e) => e,
    }
}

#[test]
fn agent_action_open_page() {
    let mut p = load();
    let desc = p.describe();
    assert!(desc.contains("\"open_page\""), "open_page is on the semantic port");
    let r = p.act(r#"{"action": "open_page", "params": {"name": "sheet-demo"}}"#);
    assert!(r.contains("\"ok\":true"), "{r}");
    assert_eq!(navigations(&mut p), vec!["sheet-demo".to_string()]);
    let st = p.cell_value("wide.state").expect("state");
    assert_eq!(st.get("opened").as_str(), Some("sheet-demo"));

    for bad in [r#"{"name": "no-such-page"}"#, r#"{"name": "../etc"}"#, r#"{"name": 7}"#, r#"{}"#] {
        let r = p.act(&format!(r#"{{"action": "open_page", "params": {bad}}}"#));
        assert!(r.contains("\"error\""), "{bad}: {r}");
        assert!(navigations(&mut p).is_empty(), "{bad} navigated");
    }
    // the page is still fine afterwards
    let d = display(&mut p, WIDE.0);
    assert!(!texts(&d).is_empty());
}

#[test]
fn missing_modules_are_shown_and_block_their_pages() {
    // no pdf, no device modules
    let mut p = load_with(&natives_without(&["pdf", "devices"]));
    let list = pages(&mut p);
    let entry = |n: &str| list.iter().find(|(x, _)| x == n).map(|(_, e)| e.clone()).expect("listed");
    let pdf = entry("pdf-demo");
    assert!(!pdf.get("can_open").truthy());
    assert_eq!(pdf.get("missing").as_str(), Some("pdf"));
    // devices are optional there: it still opens, with what is missing named
    let dev = entry("devices-demo");
    assert!(dev.get("can_open").truthy());
    assert_eq!(split(dev.get("absent_optional").str_or("")), split("ble,usb,serial"));
    assert!(entry("sheet-demo").get("can_open").truthy());
    let have = split(act(&mut p, "list_pages", Data::Nil).get("modules").str_or(""));
    assert!(!have.contains("pdf") && !have.contains("usb") && have.contains("sheet"), "{have:?}");

    // refused through the port, by keyboard and by pointer, with a message
    let r = p.act(r#"{"action": "open_page", "params": {"name": "pdf-demo"}}"#);
    assert!(r.contains("\"error\"") && r.contains("pdf"), "{r}");
    let nodes = describe(&mut p, "wide");
    let c = node(&nodes, "card:pdf-demo");
    assert!(!c.get("can_open").truthy());
    click(&mut p, center(&c));
    assert!(navigations(&mut p).is_empty(), "a page without its module was opened");
    let u = p.cell_value("wide.ui").expect("ui");
    let toasts: Vec<String> = u.get("toasts").items().iter().map(|t| t.get("text").str_or("").to_string()).collect();
    assert!(toasts.iter().any(|t| t.contains("pdf")), "no message about the missing module: {toasts:?}");
    // the devices page opens
    click(&mut p, center(&node(&nodes, "card:devices-demo")));
    assert_eq!(navigations(&mut p), vec!["devices-demo".to_string()]);
    let d = display(&mut p, WIDE.0);
    assert_no_error_card(&p, &d, "wide without pdf");
    assert!(texts(&d).iter().any(|s| s == "無法開啟"), "the blocked card is marked");

    // nothing but the UI library: every module page is marked, nothing fails
    let mut bare = load_with(&natives_without(&["demo", "pdf", "sheet", "cad", "cloud", "media", "devices", "scene"]));
    let list = pages(&mut bare);
    for (n, e) in &list {
        let needs = !e.get("modules").str_or("").is_empty();
        assert_eq!(e.get("can_open").truthy(), !needs, "{n}: {e:?}");
    }
    for x in [WIDE.0, NARROW.0] {
        let d = display(&mut bare, x);
        assert_no_error_card(&bare, &d, "no modules");
    }
    assert!(bare.key("right") && bare.key("enter"));
    let first = &list[0];
    let opened = navigations(&mut bare);
    if first.1.get("can_open").truthy() {
        assert_eq!(opened, vec![first.0.clone()]);
    } else {
        assert!(opened.is_empty());
    }
}

/// The launcher's catalog against the packages in `pages/`: every package
/// is listed, every listed one exists, and the modules match its page.json.
#[test]
fn catalog_matches_the_pages_folder() {
    let mut p = load();
    let list = pages(&mut p);
    let listed: BTreeSet<String> = list.iter().map(|(n, _)| n.clone()).collect();
    let mut on_disk = BTreeSet::new();
    let dir = root().join("pages");
    let mut problems = Vec::new();
    for entry in std::fs::read_dir(&dir).expect("pages folder").flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()).map(str::to_string) else { continue };
        let Ok(text) = std::fs::read_to_string(path.join("page.json")) else { continue };
        if name == "home" {
            continue;
        }
        on_disk.insert(name.clone());
        let m = match Manifest::parse(&text) {
            Ok(m) => m,
            Err(e) => {
                problems.push(format!("{name}: {e}"));
                continue;
            }
        };
        let Some((_, e)) = list.iter().find(|(n, _)| *n == name) else {
            problems.push(format!("{name} is in pages/ but not in pages/home/catalog.luau"));
            continue;
        };
        // required: capabilities.modules plus the modules of named streams, jobs and tiles
        let mut need: BTreeSet<String> = m.caps.modules.iter().cloned().collect();
        for n in m.caps.streams.iter().chain(&m.caps.jobs).chain(&m.caps.tiles) {
            if let Some((module, _)) = n.split_once('.') {
                need.insert(module.to_string());
            }
        }
        let opt: BTreeSet<String> = m.caps.optional_modules.iter().cloned().collect();
        let (cm, co) = (split(e.get("modules").str_or("")), split(e.get("optional").str_or("")));
        if cm != need {
            problems.push(format!("{name}: catalog modules {cm:?}, page.json needs {need:?}"));
        }
        if co != opt {
            problems.push(format!("{name}: catalog optional {co:?}, page.json optional_modules {opt:?}"));
        }
    }
    for n in listed.difference(&on_disk) {
        problems.push(format!("{n} is in the catalog but pages/{n}/page.json does not exist"));
    }
    assert!(problems.is_empty(), "catalog and pages/ disagree:\n{}", problems.join("\n"));

    // the home page declares every module the app has, so it can see which exist
    let m = &p.manifest;
    for module in anypage_std::MODULES {
        if module != "ui" {
            assert!(m.caps.optional_modules.iter().any(|x| x == module), "page.json should list {module} in optional_modules");
        }
    }
    assert!(m.caps.modules.is_empty(), "the launcher must load everywhere: no required modules");
}

#[test]
fn view_and_raster_timings() {
    let mut p = load();
    let tr = text();
    for (x, app) in [(WIDE.0, "wide"), (NARROW.0, "narrow")] {
        if app == "narrow" {
            act(&mut p, "set_layout", Data::map([("layout", Data::str("narrow"))]));
        }
        // first draw
        let t0 = Instant::now();
        let d = display(&mut p, x);
        let first = t0.elapsed().as_secs_f64() * 1000.0;
        // a focus move redraws the view (a hover does the same amount of work)
        assert!(p.key("home"));
        let mut views = Vec::new();
        for k in 0..24 {
            assert!(p.key(if k % 2 == 0 { "right" } else { "left" }));
            let t = Instant::now();
            display(&mut p, x);
            views.push(t.elapsed().as_secs_f64() * 1000.0);
        }
        let (_, r) = panel_cells(&p, x);
        // what the engine does after each change: raster the list again at
        // the camera's level (glyphs cached after the first time)
        let only = |keep_text: bool| DisplayList {
            cmds: Data::list(d.cmds.items().iter().filter(|c| (c.at(1).as_str() == Some("text")) == keep_text).cloned().collect()),
            newest: None,
        };
        let (texts_only, shapes_only) = (only(true), only(false));
        let unclipped = DisplayList {
            cmds: Data::list(d.cmds.items().iter().filter(|c| !matches!(c.at(1).as_str(), Some("clip" | "unclip"))).cloned().collect()),
            newest: None,
        };
        let mut rasters = Vec::new();
        for scale in [1.0f32, 2.0, 4.0] {
            let ms = |list: &DisplayList| {
                anypage_runtime::draw::render(list, r.w, r.h, scale, &tr);
                let t = Instant::now();
                let img = anypage_runtime::draw::render(list, r.w, r.h, scale, &tr);
                (t.elapsed().as_secs_f64() * 1000.0, img.w, img.h)
            };
            let (all, w, h) = ms(&d);
            let (txt, _, _) = ms(&texts_only);
            let (shp, _, _) = ms(&shapes_only);
            let (unc, _, _) = ms(&unclipped);
            rasters.push(format!("x{scale} {w}x{h}: {all:.1} ms (text {txt:.1}, shapes {shp:.1}, without clips {unc:.1})"));
        }
        let v = median(views);
        println!("{app}: first view {first:.2} ms, view after a focus move p50 {v:.2} ms; raster {}", rasters.join(", "));
        assert!(v < p.manifest.limits.call_ms * 0.5, "{app}: the view takes {v:.1} ms, half the budget is {}", p.manifest.limits.call_ms * 0.5);
    }
    println!("script memory {:.2} MB", p.script_memory() as f64 / 1048576.0);
    assert!(p.script_memory() < (p.manifest.limits.memory_mb as usize) << 20);
}
