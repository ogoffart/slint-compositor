// Minimal reproducer for a Slint runtime "Recursion detected" panic (Slint 1.16).
//
// A property reads its element's own `absolute-position` (which depends on the
// layout) and a `changed` handler forces that property to be evaluated. Inside a
// `for` repeater within a ScrollView, the static binding-loop analysis does NOT
// flag it (no geometry property statically depends on it), so instead of the
// usual compile-time "binding loop" error it panics at *render* time:
//
//   thread 'main' panicked at i-slint-core/.../properties.rs:583: Recursion detected
//     ... InnerRepro::item_geometry ...
//
// Compare: making a geometry property (e.g. `height`) depend on
// `absolute-position` directly IS caught at compile time as a binding loop — so
// the bug is that the same cycle laundered through a repeater + `changed`
// handler reaches a runtime panic instead of being detected (or broken) cleanly.
//
// Run: cargo run --example recursion
use slint::platform::software_renderer::{
    MinimalSoftwareWindow, PremultipliedRgbaColor, RepaintBufferType,
};
use slint::platform::{Platform, WindowAdapter};
use std::rc::Rc;

slint::slint! {
    import { ScrollView } from "std-widgets.slint";
    export component Repro inherits Window {
        width: 200px;
        height: 200px;
        in-out property <int> marker;
        ScrollView {
            VerticalLayout {
                for i in 1: r := Rectangle {
                    height: 40px;
                    property <bool> hit: r.absolute-position.y > 1000px;
                    changed hit => { root.marker += 1; }
                }
            }
        }
    }
}

struct P {
    w: Rc<MinimalSoftwareWindow>,
}
impl Platform for P {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
        Ok(self.w.clone())
    }
}

fn main() {
    let w = MinimalSoftwareWindow::new(RepaintBufferType::ReusedBuffer);
    slint::platform::set_platform(Box::new(P { w: w.clone() })).unwrap();
    w.set_size(slint::PhysicalSize::new(200, 200));
    let c = Repro::new().unwrap();
    c.show().unwrap();
    w.window().request_redraw();
    let mut buf = vec![PremultipliedRgbaColor::default(); 200 * 200];
    w.draw_if_needed(|r| {
        r.render(&mut buf, 200);
    });
    println!("rendered without panic");
}
