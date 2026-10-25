//! STEP to mesh (feature `step`): solids written by truck's own STEP writer
//! are read back through `cad.step_to_mesh` and the job `cad.step_mesh`;
//! the meshes are checked by their bounds and enclosed volume.
#![cfg(feature = "step")]

mod common;

use std::time::{Duration, Instant};

use anypage_core::Status;
use anypage_engine::Page;
use common::*;
use truck_modeling::*;

fn step_of(solid: &Solid) -> Vec<u8> {
    let compressed = solid.compress();
    let display = truck_stepio::out::CompleteStepDisplay::new(truck_stepio::out::StepModel::from(&compressed), Default::default());
    display.to_string().into_bytes()
}

fn cube() -> Vec<u8> {
    let v = builder::vertex(Point3::new(0.0, 0.0, 0.0));
    let e = builder::tsweep(&v, Vector3::unit_x());
    let f = builder::tsweep(&e, Vector3::unit_y());
    step_of(&builder::tsweep(&f, Vector3::unit_z()))
}

fn cylinder() -> Vec<u8> {
    let v = builder::vertex(Point3::new(1.0, 0.0, 0.0));
    let w: Wire = builder::rsweep(&v, Point3::origin(), Vector3::unit_z(), Rad(7.0));
    let f = builder::try_attach_plane(&[w]).expect("disc");
    step_of(&builder::tsweep(&f, Vector3::new(0.0, 0.0, 2.0)))
}

const MESH_CHECKS: &str = r#"
    -- bounds and enclosed volume (divergence theorem) of a mesh form
    function mesh_stats(m)
        local pos, idx = m.positions, m.indices
        local n = buffer.len(idx) // 4
        local lo, hi = { math.huge, math.huge, math.huge }, { -math.huge, -math.huge, -math.huge }
        for i = 0, buffer.len(pos) // 4 - 1 do
            local v = buffer.readf32(pos, i * 4)
            local k = i % 3 + 1
            lo[k] = math.min(lo[k], v)
            hi[k] = math.max(hi[k], v)
        end
        local function p(i)
            local o = buffer.readu32(idx, i * 4) * 12
            return buffer.readf32(pos, o), buffer.readf32(pos, o + 4), buffer.readf32(pos, o + 8)
        end
        local vol = 0
        for t = 0, n // 3 - 1 do
            local ax, ay, az = p(t * 3)
            local bx, by, bz = p(t * 3 + 1)
            local cx, cy, cz = p(t * 3 + 2)
            vol += (ax * (by * cz - bz * cy) - ay * (bx * cz - bz * cx) + az * (bx * cy - by * cx)) / 6
        end
        assert(buffer.len(m.normals) == buffer.len(pos), "a normal per vertex")
        return lo, hi, math.abs(vol), n // 3
    end
"#;

#[test]
fn step_solids_to_meshes() {
    let src = format!(
        r##"page.panel({{ rect = {{ 0, 0, 10, 10 }}, draw = function(d) end }})
        {MESH_CHECKS}
        local m = cad.step_to_mesh(test.bytes("cube"))
        local lo, hi, vol, tris = mesh_stats(m)
        assert(tris >= 12, "cube triangles " .. tris)
        assert(math.abs(lo[1]) < 1e-5 and math.abs(hi[3] - 1) < 1e-5, "cube bounds")
        assert(math.abs(vol - 1) < 1e-4, "cube volume " .. vol)
        assert(m.shells == 1 and #m.failed == 0)

        local c = cad.step_to_mesh(test.bytes("cylinder"))
        lo, hi, vol, tris = mesh_stats(c)
        assert(math.abs(lo[1] + 1) < 1e-3 and math.abs(hi[3] - 2) < 1e-5, "cylinder bounds")
        assert(math.abs(vol - 2 * math.pi) / (2 * math.pi) < 0.01, "cylinder volume " .. vol)
        -- a finer tolerance gives more triangles
        local fine = cad.step_to_mesh(test.bytes("cylinder"), {{ tolerance = 0.0005 }})
        assert(buffer.len(fine.indices) > buffer.len(c.indices))

        local ok, err = pcall(cad.step_to_mesh, "not step")
        assert(not ok and string.find(tostring(err), "ISO%-10303%-21"), tostring(err))
        Job = page.job("mesh", "cad.step_mesh", {{ page.input("bytes", test.bytes("cylinder")) }})
        "##
    );
    let n = natives(vec![("cube", cube()), ("cylinder", cylinder())]);
    let t = Instant::now();
    let mut p = load_page(&src, &n).unwrap_or_else(|e| panic!("{e}"));
    eprintln!("STEP page (3 meshes) in {:.0} ms", t.elapsed().as_secs_f64() * 1000.0);
    let cell = p.cell_id("mesh").unwrap();
    let t = Instant::now();
    loop {
        let g = p.parts().0;
        g.commit();
        let i = g.demand(cell);
        if matches!(i.status, Status::Ready) {
            let d = anypage_runtime::data::input_to_data(&i);
            assert!(d.get("triangles").num().unwrap() > 20.0, "{d:?}");
            break;
        }
        assert!(!matches!(i.status, Status::Error(_)), "{:?}", i.status);
        assert!(t.elapsed() < Duration::from_secs(20), "job timed out");
        std::thread::sleep(Duration::from_millis(10));
    }
    p.shutdown();
}

/// The cube rewritten the way commercial AP214 exporters write it: ADVANCED_FACE
/// with FACE_OUTER_BOUND, colours through styled items, and an assembly
/// placement as a complex entity instance. None of the extras may stop the read.
fn commercial_style_cube() -> Vec<u8> {
    let text = String::from_utf8(cube()).unwrap();
    let text = text.replace("FACE_SURFACE(", "ADVANCED_FACE(").replace("FACE_BOUND(", "FACE_OUTER_BOUND(");
    let axis = text
        .lines()
        .find(|l| l.contains("AXIS2_PLACEMENT_3D("))
        .and_then(|l| l.split('=').next())
        .map(|s| s.trim().to_string())
        .expect("an axis placement");
    let extras = format!(
        "#900 = MECHANICAL_DESIGN_GEOMETRIC_PRESENTATION_REPRESENTATION('', (#901), #11);
         #901 = STYLED_ITEM('color', (#902), #16);
         #902 = PRESENTATION_STYLE_ASSIGNMENT((#903));
         #903 = SURFACE_STYLE_USAGE(.BOTH., #904);
         #904 = SURFACE_SIDE_STYLE('', (#905));
         #905 = SURFACE_STYLE_FILL_AREA(#906);
         #906 = FILL_AREA_STYLE('', (#907));
         #907 = FILL_AREA_STYLE_COLOUR('', #908);
         #908 = COLOUR_RGB('', 0.8, 0.6, 0.2);
         #950 = ( REPRESENTATION_RELATIONSHIP('', '', #10, #10) REPRESENTATION_RELATIONSHIP_WITH_TRANSFORMATION(#951) SHAPE_REPRESENTATION_RELATIONSHIP() );
         #951 = ITEM_DEFINED_TRANSFORMATION('', '', {axis}, {axis});
         #952 = PRODUCT_RELATED_PRODUCT_CATEGORY('part', $, (#7));
"
    );
    let at = text.rfind("ENDSEC;").expect("data section end");
    let mut out = text[..at].to_string();
    out.push_str(&extras);
    out.push_str(&text[at..]);
    out.into_bytes()
}

#[test]
fn commercial_style_wrappers_are_tolerated() {
    let bytes = commercial_style_cube();
    std::fs::write(out_dir().join("cube_ap214_style.step"), &bytes).unwrap();
    let src = format!(
        r##"page.panel({{ rect = {{ 0, 0, 10, 10 }}, draw = function(d) end }})
        {MESH_CHECKS}
        local m = cad.step_to_mesh(test.bytes("cube"))
        local lo, hi, vol = mesh_stats(m)
        assert(math.abs(vol - 1) < 1e-4, "volume " .. vol)
        "##
    );
    let n = natives(vec![("cube", bytes)]);
    load_page(&src, &n).unwrap_or_else(|e| panic!("{e}"));
}

/// The function runs on the page's thread: large files go to the job.
#[test]
fn large_files_are_sent_to_the_job() {
    let mut big = cube();
    big.extend(std::iter::repeat_n(b' ', 5 << 20));
    let n = natives(vec![("big", big)]);
    let src = "page.panel({ rect = { 0, 0, 10, 10 }, draw = function(d) end })\nlocal ok, err = pcall(cad.step_to_mesh, test.bytes('big'))\nassert(not ok and string.find(tostring(err), 'cad.step_mesh'), tostring(err))";
    load_page(src, &n).unwrap_or_else(|e| panic!("{e}"));
}
