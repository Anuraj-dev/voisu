// Release builds are GUI-subsystem so no console window opens with the tray app.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    voisu_windows::run();
}
