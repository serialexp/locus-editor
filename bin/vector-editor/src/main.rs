mod app;
mod camera;
mod context_menu;
mod demo;
mod editor_state;
mod gradient_editor;
mod hud;
mod paint_picker;
mod properties_panel;
mod snap;
mod structure_panel;
mod trace_dialog;
mod ui;
mod util;

use app::App;

fn main() {
    env_logger::init();

    let event_loop = winit::event_loop::EventLoop::new().expect("failed to create event loop");
    event_loop.set_control_flow(winit::event_loop::ControlFlow::Wait);

    let mut app = App::default();
    event_loop.run_app(&mut app).expect("event loop failed");
}
